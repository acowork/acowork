/**
 * Unit tests for the user↔user inbox store (ADR-076 §决策 8).
 *
 * The inbox is the only path where a human-authored message reaches the
 * wire, so pin the three things that would silently corrupt history:
 *
 *   1. the canonical `min__max` chat id — a flipped order would file the
 *      same pair under a second conversation and split the thread,
 *   2. the send path trusting the Gateway's echo (never a locally minted
 *      message — the server owns `ts` and `from`),
 *   3. the read receipt clearing the badge, and a failed send *keeping*
 *      the draft instead of losing the user's text.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

vi.mock("../lib/config", () => ({ getGatewayUrl: () => "http://gw.test" }));
vi.mock("../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));

import { useAuthStore } from "./authStore";
import {
  POLL_MS,
  chatIdOf,
  syncPollLoop,
  useUserChatStore,
} from "./userChatStore";
import type { UserAccount } from "../lib/types";

/** Deliberately the *larger* id, so a flipped pair would be caught. */
const SELF = "bbbbbbbb-0000-4000-8000-000000000002";
const PEER = "aaaaaaaa-0000-4000-8000-000000000001";
const CHAT = `${PEER}__${SELF}`;

const self: UserAccount = {
  user_id: SELF,
  username: "bob",
  display_name: "Bob",
  role: "admin",
  language: "en",
  timezone: "UTC",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
};

const IMAGE = {
  id: "11111111-0000-4000-8000-000000000001",
  filename: "shot.png",
  mime: "image/png",
  size: 2048,
};
const DOC = {
  id: "22222222-0000-4000-8000-000000000002",
  filename: "spec.pdf",
  mime: "application/pdf",
  size: 4096,
};

function file(name: string): File {
  return new File([new Uint8Array([1, 2, 3])], name, { type: "image/png" });
}

function json(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
  } as unknown as Response;
}

interface Route {
  method?: string;
  suffix: string;
  make: (init?: RequestInit) => Response;
}

/** Route-aware fetch stub so each test can assert the exact URL shape. */
function stubFetch(routes: Route[]) {
  const spy = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input.toString();
    const method = init?.method ?? "GET";
    for (const route of routes) {
      if (!url.includes(route.suffix)) continue;
      if (route.method && route.method !== method) continue;
      return route.make(init);
    }
    throw new Error(`unexpected fetch: ${method} ${url}`);
  });
  globalThis.fetch = spy as unknown as typeof fetch;
  return spy;
}

function summary(unread: number) {
  return {
    chat_id: CHAT,
    peer_user_id: PEER,
    peer_display_name: "Alice",
    last_active_at: 100,
    last_message_preview: "hello",
    unread_count: unread,
  };
}

beforeEach(() => {
  localStorage.clear();
  useUserChatStore.getState().reset();
  // Installing the absorbing stub *before* the auth flip matters: signing in
  // arms the session-owned poll loop, which refreshes immediately. Without
  // this the bootstrap call would land on the previous test's fetch spy and
  // could leave a stale `error` in the state a test then asserts against.
  globalThis.fetch = vi.fn(
    async () => json({ chats: [], messages: [], total: 0, offset: 0, limit: 50 }),
  ) as unknown as typeof fetch;
  useAuthStore.setState({
    mode: "multi_user",
    status: "logged_in",
    account: self,
    accessToken: "at",
  });
});

describe("chatIdOf", () => {
  it("is order-independent and byte-sorted", () => {
    expect(chatIdOf(SELF, PEER)).toBe(CHAT);
    expect(chatIdOf(PEER, SELF)).toBe(CHAT);
  });
});

describe("useUserChatStore", () => {
  it("loads the signed-in user's conversations", async () => {
    const spy = stubFetch([{ suffix: "/chats", make: () => json({ chats: [summary(2)] }) }]);

    await useUserChatStore.getState().refreshChats();

    expect(useUserChatStore.getState().chats).toHaveLength(1);
    expect(useUserChatStore.getState().chats[0].unread_count).toBe(2);
    expect(String(spy.mock.calls[0][0])).toBe(`http://gw.test/api/users/${SELF}/chats`);
  });

  it("opens a thread and loads it WITHOUT acking — opening is not reading", async () => {
    const spy = stubFetch([
      {
        suffix: "/messages",
        method: "GET",
        make: () =>
          json({
            chat_id: CHAT,
            messages: [{ ts: 10, from: PEER, kind: "text", body: "hi" }],
            total: 1,
            offset: 0,
            limit: 50,
          }),
      },
      { suffix: "/read", method: "POST", make: () => json(null, 204) },
      { suffix: "/chats", make: () => json({ chats: [summary(3)] }) },
    ]);

    await useUserChatStore.getState().refreshChats();
    await useUserChatStore.getState().openChat(PEER);

    const state = useUserChatStore.getState();
    expect(state.activePeerId).toBe(PEER);
    expect(state.messages.map((m) => m.body)).toEqual(["hi"]);
    // The receipt is the renderer's call (`markThreadRead`), so merely
    // opening must leave the badge alone.
    expect(state.chats[0].unread_count).toBe(3);
    expect(
      spy.mock.calls.filter(([, init]) => init?.method === "POST"),
    ).toHaveLength(0);
  });

  it("keeps the Gateway's echo of a sent message and refetches the list", async () => {
    const spy = stubFetch([
      {
        suffix: "/messages",
        method: "GET",
        make: () =>
          json({ chat_id: CHAT, messages: [], total: 0, offset: 0, limit: 50 }),
      },
      {
        suffix: "/messages",
        method: "POST",
        make: (init) => {
          const sent = JSON.parse(String(init?.body)) as { body: string };
          // The server owns ts/from/kind — the store must not mint them.
          return json({ ts: 500, from: SELF, kind: "text", body: sent.body }, 201);
        },
      },
      // Last: `/api/users/{id}/chats/{chat}/messages` also contains "/chats".
      { suffix: "/chats", make: () => json({ chats: [summary(0)] }) },
    ]);

    await useUserChatStore.getState().refreshChats();
    await useUserChatStore.getState().openChat(PEER);
    const ok = await useUserChatStore.getState().send("  hello alice  ");

    expect(ok).toBe(true);
    const { messages } = useUserChatStore.getState();
    expect(messages).toHaveLength(1);
    expect(messages[0]).toMatchObject({ ts: 500, from: SELF, body: "hello alice" });

    await vi.waitFor(() => {
      const listCalls = spy.mock.calls.filter(
        ([url, init]) => String(url).endsWith("/chats") && init?.method === undefined,
      );
      expect(listCalls.length).toBeGreaterThanOrEqual(2);
    });
  });

  it("reports a failed send and keeps the thread intact", async () => {
    stubFetch([
      {
        suffix: "/messages",
        method: "GET",
        make: () =>
          json({ chat_id: CHAT, messages: [], total: 0, offset: 0, limit: 50 }),
      },
      {
        suffix: "/messages",
        method: "POST",
        make: () => json({ error: "recipient account not found" }, 404),
      },
      { suffix: "/chats", make: () => json({ chats: [] }) },
    ]);

    await useUserChatStore.getState().openChat(PEER);
    const ok = await useUserChatStore.getState().send("hello?");

    expect(ok).toBe(false);
    expect(useUserChatStore.getState().error).toBe("recipient account not found");
    expect(useUserChatStore.getState().messages).toHaveLength(0);
    expect(useUserChatStore.getState().sending).toBe(false);
  });

  it("does nothing when the account system is off", async () => {
    const spy = stubFetch([]);
    useAuthStore.setState({ mode: "local", status: "disabled", account: null });

    await useUserChatStore.getState().refreshChats();

    expect(spy).not.toHaveBeenCalled();
    expect(useUserChatStore.getState().chats).toHaveLength(0);
  });

  it("uploads a file to the conversation's own files route", async () => {
    const spy = stubFetch([
      {
        suffix: "/messages",
        method: "GET",
        make: () =>
          json({ chat_id: CHAT, messages: [], total: 0, offset: 0, limit: 50 }),
      },
      { suffix: "/files", method: "POST", make: () => json(IMAGE, 201) },
    ]);

    await useUserChatStore.getState().openChat(PEER);
    const ok = await useUserChatStore.getState().attach(file("shot.png"));

    expect(ok).toBe(true);
    expect(useUserChatStore.getState().pendingAttachments).toEqual([IMAGE]);
    expect(useUserChatStore.getState().uploading).toEqual([]);

    const call = spy.mock.calls.find(
      ([url, init]) => String(url).endsWith("/files") && init?.method === "POST",
    );
    expect(String(call?.[0])).toBe(
      `http://gw.test/api/users/${SELF}/chats/${CHAT}/files`,
    );
    // FormData sets its own boundary; setting Content-Type by hand breaks it.
    // (`authFetch` will add Authorization, which is fine — that is its job.)
    expect(call?.[1]?.body).toBeInstanceOf(FormData);
    expect(
      (call?.[1]?.headers as Record<string, string> | undefined)?.[
        "Content-Type"
      ],
    ).toBeUndefined();
  });

  it("sends pending attachments by id and only clears what went out", async () => {
    const spy = stubFetch([
      {
        suffix: "/messages",
        method: "GET",
        make: () =>
          json({ chat_id: CHAT, messages: [], total: 0, offset: 0, limit: 50 }),
      },
      { suffix: "/files", method: "POST", make: () => json(IMAGE, 201) },
      {
        suffix: "/messages",
        method: "POST",
        make: () =>
          json(
            {
              ts: 900,
              from: SELF,
              kind: "image",
              body: "",
              // The Gateway echoes back its own record, not what we sent.
              attachments: [IMAGE],
            },
            201,
          ),
      },
      { suffix: "/chats", make: () => json({ chats: [] }) },
    ]);

    await useUserChatStore.getState().openChat(PEER);
    await useUserChatStore.getState().attach(file("shot.png"));
    // An image with no caption is a message.
    const ok = await useUserChatStore.getState().send("   ");

    expect(ok).toBe(true);
    expect(useUserChatStore.getState().pendingAttachments).toEqual([]);
    expect(useUserChatStore.getState().messages[0].attachments).toEqual([IMAGE]);

    const post = spy.mock.calls.find(
      ([url, init]) =>
        String(url).endsWith("/messages") && init?.method === "POST",
    );
    expect(JSON.parse(String(post?.[1]?.body))).toEqual({
      body: "",
      attachments: [IMAGE.id],
    });
  });

  it("keeps attachments queued when the send is refused", async () => {
    stubFetch([
      {
        suffix: "/messages",
        method: "GET",
        make: () =>
          json({ chat_id: CHAT, messages: [], total: 0, offset: 0, limit: 50 }),
      },
      { suffix: "/files", method: "POST", make: () => json(DOC, 201) },
      {
        suffix: "/messages",
        method: "POST",
        make: () => json({ error: "attachment is empty" }, 422),
      },
      { suffix: "/chats", make: () => json({ chats: [] }) },
    ]);

    await useUserChatStore.getState().openChat(PEER);
    await useUserChatStore.getState().attach(file("spec.pdf"));
    const ok = await useUserChatStore.getState().send("here");

    expect(ok).toBe(false);
    // Uploading cost the user a round trip; a refused send must not drop it.
    expect(useUserChatStore.getState().pendingAttachments).toEqual([DOC]);
  });

  it("drops queued attachments when the thread is switched", async () => {
    stubFetch([
      {
        suffix: "/messages",
        method: "GET",
        make: () =>
          json({ chat_id: CHAT, messages: [], total: 0, offset: 0, limit: 50 }),
      },
      { suffix: "/files", method: "POST", make: () => json(IMAGE, 201) },
      { suffix: "/read", method: "POST", make: () => json(null, 204) },
    ]);

    await useUserChatStore.getState().openChat(PEER);
    await useUserChatStore.getState().attach(file("shot.png"));
    expect(useUserChatStore.getState().pendingAttachments).toHaveLength(1);

    await useUserChatStore.getState().openChat("cccccccc-0000-4000-8000-000000000003");
    // They belong to the peer that was open, not to the composer in general.
    expect(useUserChatStore.getState().pendingAttachments).toEqual([]);
  });

  it("surfaces a rejected upload without queueing it", async () => {
    stubFetch([
      {
        suffix: "/messages",
        method: "GET",
        make: () =>
          json({ chat_id: CHAT, messages: [], total: 0, offset: 0, limit: 50 }),
      },
      {
        suffix: "/files",
        method: "POST",
        make: () => json({ error: "attachment exceeds 26214400 bytes" }, 413),
      },
    ]);

    await useUserChatStore.getState().openChat(PEER);
    const ok = await useUserChatStore.getState().attach(file("huge.png"));

    expect(ok).toBe(false);
    expect(useUserChatStore.getState().pendingAttachments).toEqual([]);
    expect(useUserChatStore.getState().error).toContain("exceeds");
    expect(useUserChatStore.getState().uploading).toEqual([]);
  });
});

describe("session-owned poll loop", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("polls with zero components mounted and dies with the session", async () => {
    // The `beforeEach` auth flip armed a *real* interval; disarm it and
    // re-arm under fake timers, so the assertions below drive the loop the
    // same way a running app does — by changing the session, never by
    // mounting a view.
    vi.useRealTimers();
    useAuthStore.setState({ mode: "local", account: null });
    vi.useFakeTimers();

    const spy = stubFetch([
      { suffix: "/chats", make: () => json({ chats: [summary(1)] }) },
    ]);

    useAuthStore.setState({
      mode: "multi_user",
      status: "logged_in",
      account: self,
      accessToken: "at",
    });
    // Arming refreshes immediately rather than making the user wait out a
    // full interval.
    const armed = spy.mock.calls.length;
    expect(armed).toBeGreaterThan(0);

    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(spy.mock.calls.length).toBe(armed + 1);

    // Sign out. Nothing here is a component unmount — this is the whole
    // point: the loop's lifetime is the session's lifetime.
    useAuthStore.setState({ mode: "local", account: null });
    const stopped = spy.mock.calls.length;
    await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    expect(spy.mock.calls.length).toBe(stopped);
  });

  it("polls the open thread too, and only that", async () => {
    vi.useRealTimers();
    useAuthStore.setState({ mode: "local", account: null });
    vi.useFakeTimers();

    const spy = stubFetch([
      { suffix: "/chats", make: () => json({ chats: [summary(1)] }) },
      {
        suffix: "/messages",
        method: "GET",
        make: () =>
          json({
            chat_id: CHAT,
            messages: [{ ts: 10, from: PEER, kind: "text", body: "hi" }],
            total: 1,
            offset: 0,
            limit: 50,
          }),
      },
    ]);

    useAuthStore.setState({
      mode: "multi_user",
      status: "logged_in",
      account: self,
      accessToken: "at",
    });
    await useUserChatStore.getState().openChat(PEER);
    spy.mockClear();

    await vi.advanceTimersByTimeAsync(POLL_MS);

    const urls = spy.mock.calls.map(([u]) => String(u));
    expect(urls.some((u) => u.includes("/chats"))).toBe(true);
    // The regression this replaces: an agent session being selected used to
    // unmount the inbox, so this GET never happened and the message from
    // another device simply never arrived.
    expect(urls.some((u) => u.includes(`/chats/${CHAT}/messages`))).toBe(true);
  });

  it("is idempotent across repeated auth churn", () => {
    vi.useRealTimers();
    for (let i = 0; i < 4; i += 1) {
      useAuthStore.setState({ mode: "multi_user", account: self, status: "logged_in", accessToken: "at" });
      useAuthStore.setState({ mode: "local", account: null });
    }
    // No leaked interval: re-arming a stopped loop is the only way one exists.
    useAuthStore.setState({ mode: "multi_user", account: self, status: "logged_in", accessToken: "at" });
    syncPollLoop();
    expect(() => syncPollLoop()).not.toThrow();
    useAuthStore.setState({ mode: "local", account: null });
  });
});

describe("markThreadRead", () => {
  /** Load a one-message thread the way the renderer would see it. */
  async function showThread(unread: number, readStatus = 204) {
    const spy = stubFetch([
      {
        suffix: "/messages",
        method: "GET",
        make: () =>
          json({
            chat_id: CHAT,
            messages: [{ ts: 10, from: PEER, kind: "text", body: "hi" }],
            total: 1,
            offset: 0,
            limit: 50,
          }),
      },
      { suffix: "/read", method: "POST", make: () => json(readStatus === 204 ? null : { error: "boom" }, readStatus) },
      { suffix: "/chats", make: () => json({ chats: [summary(unread)] }) },
    ]);
    await useUserChatStore.getState().refreshChats();
    await useUserChatStore.getState().openChat(PEER);
    return spy;
  }

  it("acks what the renderer showed and clears the badge", async () => {
    const spy = await showThread(1);
    spy.mockClear();

    await useUserChatStore.getState().markThreadRead();

    expect(String(spy.mock.calls[0][0])).toBe(
      `http://gw.test/api/users/${SELF}/chats/${CHAT}/read`,
    );
    expect(useUserChatStore.getState().chats[0].unread_count).toBe(0);
  });

  it("costs no request while the newest message is unchanged", async () => {
    // The poll re-sets `messages` every 5 s even when nothing arrived; the
    // receipt must not turn that into a POST every 5 s.
    const spy = await showThread(2);
    await useUserChatStore.getState().markThreadRead();
    spy.mockClear();

    await useUserChatStore.getState().markThreadRead();
    await useUserChatStore.getState().markThreadRead();

    expect(spy.mock.calls.filter(([, init]) => init?.method === "POST")).toHaveLength(0);
  });

  it("acks again when a newer message arrives", async () => {
    const spy = await showThread(1);
    await useUserChatStore.getState().markThreadRead();
    useUserChatStore.setState({
      messages: [
        { ts: 10, from: PEER, kind: "text", body: "hi" },
        { ts: 11, from: PEER, kind: "text", body: "there" },
      ],
    });
    spy.mockClear();

    await useUserChatStore.getState().markThreadRead();

    expect(spy.mock.calls.filter(([, init]) => init?.method === "POST")).toHaveLength(1);
  });

  it("retries a receipt the Gateway refused", async () => {
    const spy = await showThread(1, 500);

    await useUserChatStore.getState().markThreadRead();
    const posts = () =>
      spy.mock.calls.filter(([, init]) => init?.method === "POST").length;
    expect(posts()).toBe(1);

    spy.mockClear();
    await useUserChatStore.getState().markThreadRead();
    // A refused receipt must not be remembered as done, or the badge would
    // stay cleared locally while the server still counts it. The failure is
    // deliberately not surfaced as `error`: a background receipt is not the
    // user's problem, and a banner here would blank a thread that loaded fine.
    expect(posts()).toBe(1);
    expect(useUserChatStore.getState().error).toBeNull();
  });

  it("does nothing when no thread is open", async () => {
    const spy = stubFetch([{ suffix: "/read", method: "POST", make: () => json(null, 204) }]);
    await useUserChatStore.getState().markThreadRead();
    expect(spy).not.toHaveBeenCalled();
  });
});
