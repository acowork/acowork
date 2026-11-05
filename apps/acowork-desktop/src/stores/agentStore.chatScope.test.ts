/**
 * ADR-106 D7: the `@` picker's list is `GET …/sessions` narrowed by
 * `chat_session_id` / `surface` — one list, one filter, no second endpoint.
 *
 * What is pinned here is the plumbing the picker depends on and cannot see:
 *  - the scope actually goes on the wire — without it the Runtime would hand
 *    back the agent's whole history, i.e. runs this chat may not address;
 *  - rows land in a slice keyed by agent + surface + chat, so two chats of one
 *    agent never share a page cursor (and neither shares the sidebar's);
 *  - the server's order survives: the listing is most-recently-ACTIVE first,
 *    which is what the router's own candidate rule sorts by, so a client
 *    re-sort by creation would disagree with the address being delivered;
 *  - a failed read keeps the rows (ADR-085) and a late answer for a superseded
 *    page cannot repaint the newer one.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// Keep the Tauri invoke stub — agentStore imports it at module load.
vi.mock("@tauri-apps/api/core", () => ({
  invoke: () => Promise.reject(new Error("not used in this test")),
}));

vi.mock("../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
  setLevel: () => {},
  getLevel: () => "off" as const,
}));

// Profile persistence is loaded at module init.
vi.mock("../lib/profileStore", () => ({
  loadAllProfiles: () => ({}),
  loadProfile: () => null,
  saveProfile: () => {},
  DEFAULT_PROFILE: {
    language: "zh-CN",
    timezone: "Asia/Shanghai",
    model_override: null,
    reasoning_effort: null,
    auto_approve_tools: [],
    yolo_mode: false,
  },
}));

// ── SUT ──────────────────────────────────────────────────────────────────

import { chatListKey, useAgentStore } from "./agentStore";
import { useAuthStore } from "./authStore";
import type { SessionInfo } from "../lib/types";

const AGENT = "com.test.chat_scope";
const CHAT = "cs-1";
const OTHER_CHAT = "cs-2";

function row(sessionId: string, createdAt: string): SessionInfo {
  return {
    session_id: sessionId,
    title: sessionId,
    created_at: createdAt,
    message_count: 2,
  } as SessionInfo;
}

function jsonResponse(sessions: SessionInfo[], total = sessions.length) {
  return {
    ok: true,
    status: 200,
    json: async () => ({
      sessions,
      total_count: total,
      total_pages: Math.max(1, Math.ceil(total / 10)),
    }),
    text: async () => "",
    headers: { get: () => null },
  };
}

let urls: string[] = [];

beforeEach(() => {
  urls = [];
  useAuthStore.setState({ viewAsUserId: null });
  useAgentStore.setState({ chatSessionLists: {} });
});

afterEach(() => vi.unstubAllGlobals());

describe("fetchChatSessions — the chat-scoped listing", () => {
  it("puts this chat's scope on the request and the rows in this chat's slice", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: RequestInfo | URL) => {
        urls.push(String(url));
        return jsonResponse([row("s-1", "2026-11-01T10:00:00Z")], 24);
      }),
    );

    await useAgentStore.getState().fetchChatSessions(AGENT, CHAT, "group", 2);

    const params = new URL(urls[0]).searchParams;
    // The permission narrowing is a server decision driven by these parameters:
    // drop either one and the picker is offering somebody else's work.
    expect(params.get("chat_session_id")).toBe(CHAT);
    expect(params.get("surface")).toBe("group");
    expect(params.get("page")).toBe("2");
    const list = useAgentStore.getState().chatSessionLists[chatListKey(AGENT, "group", CHAT)];
    expect(list.sessions.map((s) => s.session_id)).toEqual(["s-1"]);
    // `total_pages` is already the narrowed count (the filter runs before
    // pagination), so the pager must show the server's number, not a guess.
    expect(list.pagination).toEqual({ currentPage: 2, totalPages: 3, totalCount: 24, pageSize: 10 });
    expect(list.isLoading).toBe(false);
  });

  it("asks for its own page size, not the sidebar's", async () => {
    // The two listings are separate slices with separate questions: the sidebar
    // pages the agent's whole history (20 rows), the picker shows a handful of
    // addresses above the composer (10). Sharing one pageSize made the picker's
    // tighter page shrink the history list as a side effect.
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: RequestInfo | URL) => {
        urls.push(String(url));
        return jsonResponse([]);
      }),
    );

    await useAgentStore.getState().fetchChatSessions(AGENT, CHAT, "direct", 1);

    expect(new URL(urls[0]).searchParams.get("size")).toBe("10");
  });

  it("rides the admin's read-only view-as like the sidebar's listing does", async () => {
    useAuthStore.setState({ viewAsUserId: "u-other" });
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: RequestInfo | URL) => {
        urls.push(String(url));
        return jsonResponse([]);
      }),
    );

    await useAgentStore.getState().fetchChatSessions(AGENT, CHAT, "direct", 1);

    expect(new URL(urls[0]).searchParams.get("as_user")).toBe("u-other");
  });

  it("keeps the server's most-recently-active order", async () => {
    // Deliberately oldest-last-by-creation: the sidebar re-sorts by
    // `created_at`, this listing must not — a re-sorted page would put a
    // stale-but-early-created run where the router's own ordering says the
    // human should look first.
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        jsonResponse([row("s-active", "2026-10-01T00:00:00Z"), row("s-new", "2026-11-01T00:00:00Z")]),
      ),
    );

    await useAgentStore.getState().fetchChatSessions(AGENT, CHAT, "direct", 1);

    const list = useAgentStore.getState().chatSessionLists[chatListKey(AGENT, "direct", CHAT)];
    expect(list.sessions.map((s) => s.session_id)).toEqual(["s-active", "s-new"]);
  });

  it("pages two chats of one agent separately", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => jsonResponse([row("s-1", "2026-11-01T10:00:00Z")])),
    );

    await useAgentStore.getState().fetchChatSessions(AGENT, CHAT, "direct", 1);
    await useAgentStore.getState().fetchChatSessions(AGENT, OTHER_CHAT, "direct", 3);

    // Opening the second chat's picker cannot repaint the first: different
    // slices, so neither chat moves the other's page cursor — and the sidebar's
    // `agents[id].sessions` is a third slice, untouched by either.
    const first = useAgentStore.getState().chatSessionLists[chatListKey(AGENT, "direct", CHAT)];
    const second = useAgentStore.getState().chatSessionLists[chatListKey(AGENT, "direct", OTHER_CHAT)];
    expect(first.pagination.currentPage).toBe(1);
    expect(second.pagination.currentPage).toBe(3);
  });

  it("keeps the rows it has when a refresh fails", async () => {
    let fail = false;
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        fail
          ? { ok: false, status: 500, json: async () => ({}), text: async () => "", headers: { get: () => null } }
          : jsonResponse([row("s-1", "2026-11-01T10:00:00Z")]),
      ),
    );
    await useAgentStore.getState().fetchChatSessions(AGENT, CHAT, "direct", 1);

    fail = true;
    await useAgentStore.getState().fetchChatSessions(AGENT, CHAT, "direct", 2);

    // ADR-085: a failed read is not evidence of "nothing to continue" — an
    // emptied picker reads as a chat with no work, and invites a duplicate task.
    const list = useAgentStore.getState().chatSessionLists[chatListKey(AGENT, "direct", CHAT)];
    expect(list.sessions.map((s) => s.session_id)).toEqual(["s-1"]);
    expect(list.pagination.currentPage).toBe(1);
    expect(list.isLoading).toBe(false);
  });

  it("drops a late answer for a page the human already paged past", async () => {
    const key = chatListKey(AGENT, "direct", CHAT);
    let release: ((value: unknown) => void) | undefined;
    const gate = new Promise((resolve) => (release = resolve));
    let call = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        call += 1;
        if (call === 1) {
          await gate; // page 1 answers last
          return jsonResponse([row("s-p1", "2026-11-01T10:00:00Z")]);
        }
        return jsonResponse([row("s-p2", "2026-11-01T11:00:00Z")]);
      }),
    );

    const pageOne = useAgentStore.getState().fetchChatSessions(AGENT, CHAT, "direct", 1);
    await useAgentStore.getState().fetchChatSessions(AGENT, CHAT, "direct", 2);
    release?.(undefined);
    await pageOne;

    // The pager is a tap away from being tapped twice; the older request's
    // answer must not repaint the newer page.
    expect(useAgentStore.getState().chatSessionLists[key].sessions.map((s) => s.session_id)).toEqual([
      "s-p2",
    ]);
    expect(useAgentStore.getState().chatSessionLists[key].pagination.currentPage).toBe(2);
  });
});
