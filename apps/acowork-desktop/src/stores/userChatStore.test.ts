/**
 * Unit tests for the user↔user inbox store (ADR-076 §决策 8).
 *
 * The inbox is the only path where a human-authored message reaches the
 * wire, so pin the things that would silently corrupt history or destroy the
 * reading position:
 *
 *   1. the canonical `min__max` chat id — a flipped order would file the
 *      same pair under a second conversation and split the thread,
 *   2. the send path trusting the Gateway's echo (never a locally minted
 *      message — the server owns `ts` and `from`),
 *   3. the read receipt clearing the badge, and a failed send *keeping*
 *      the draft instead of losing the user's text,
 *   4. `planPage` — the rule that lets a re-read arrive on a signal every few
 *      seconds without moving the thread the user is reading (design doc 25 §8),
 *   5. the push/poll exclusivity, including the trap where the bridge coming
 *      up must not be mistaken for a logout.
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
  planPage,
  setChatPushChannel,
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
  // The push channel is module state: a test that raised it must not leave the
  // fallback timer stopped for every test after it.
  setChatPushChannel(false);
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

/* ---------------- the reload rule (design doc 25 §8) ---------------- */

type Msg = { ts: number; from: string; kind: "text"; body: string };

/**
 * Message objects are shared between pages on purpose: reference identity is
 * exactly what the tail-append rule hands to React, and a fresh object per
 * fetch would make "nothing re-rendered" unmeasurable.
 */
function m(ts: number, body: string, from = PEER): Msg {
  return { ts, from, kind: "text", body };
}

const M1 = m(1, "one");
const M2 = m(2, "two");
const M3 = m(3, "three");
const M4 = m(4, "four");

function pageBody(messages: Msg[], total: number | null) {
  return {
    chat_id: CHAT,
    ...(total === null ? {} : { total }),
    messages,
    offset: 0,
    limit: 50,
  };
}

describe("planPage — deprecated total-delta fallback (no `seq`)", () => {
  // These messages carry no `seq`, so `planPage` takes the legacy branch a
  // pre-ADR-092 Gateway is served by. Kept to pin that the fallback still
  // behaves exactly as the rule it replaced.
  it("does nothing at all when the count did not move", () => {
    expect(planPage([M1, M2], [M1, M2], { knownTotal: 2, total: 2 })).toEqual({ kind: "none" });
  });

  it("appends only the newest delta, and the appended items ARE the page's", () => {
    const plan = planPage([M1, M2], [M1, M2, M3, M4], { knownTotal: 2, total: 4 });
    expect(plan).toEqual({ kind: "append", tail: [M3, M4] });
    if (plan.kind !== "append") throw new Error("unreachable");
    // The head of the page is dropped precisely because the window already
    // holds those objects — re-setting it is what would move the viewport.
    expect(plan.tail[0]).toBe(M3);
    expect(plan.tail[1]).toBe(M4);
  });

  it("appends one item when a single message arrived", () => {
    expect(planPage([M1, M2], [M1, M2, M3], { knownTotal: 2, total: 3 })).toEqual({
      kind: "append",
      tail: [M3],
    });
  });

  it("resyncs when the gap is bigger than one page — the delta is unreconstructable", () => {
    const held = [M1, M2];
    const page = [M3, M4];
    expect(planPage(held, page, { knownTotal: 2, total: 60 })).toEqual({
      kind: "resync",
      messages: page,
      unseen: 58,
    });
  });

  it("resyncs when the thread shrank (a negative delta is never an append)", () => {
    const page = [M2];
    expect(planPage([M1, M2], page, { knownTotal: 5, total: 3 })).toEqual({
      kind: "resync",
      messages: page,
      unseen: 0,
    });
  });

  it("resyncs as long as no count is known, but only if the content differs", () => {
    // `undefined` never reaches here: `listMessages` normalizes a missing
    // `total` to null. That is what keeps `d` from becoming NaN and the rule
    // from appending the entire page on top of the window.
    const page = [M1, M2];
    expect(planPage([M1, M2], page, { knownTotal: 2, total: null })).toEqual({ kind: "none" });
    expect(planPage([M1], [M1, M2], { knownTotal: 2, total: null })).toEqual({
      kind: "resync",
      messages: [M1, M2],
      unseen: 0,
    });
    expect(planPage([M1, M2], page, { knownTotal: null, total: 2 })).toEqual({ kind: "none" });
  });

  it("counts a same-length page of different objects as a change", () => {
    // A re-parse of identical JSON is not reference equality, and the rule
    // must not convince itself the window is current.
    expect(planPage([m(1, "one")], [m(1, "one")], { knownTotal: 1, total: null }).kind).toBe(
      "resync",
    );
  });
});

describe("planPage — seq suffix-diff (ADR-092 decision 2)", () => {
  // Seq-bearing messages: the rule is a content judgement, not a count guess.
  function s(seq: number, body: string, from = PEER) {
    return { seq, ts: seq, from, kind: "text" as const, body };
  }
  const S1 = s(1, "one");
  const S2 = s(2, "two");
  const S3 = s(3, "three");
  const S4 = s(4, "four");

  it("appends the suffix past heldMaxSeq and keeps the held references", () => {
    const plan = planPage([S1, S2], [S1, S2, S3], { knownTotal: null, total: 3 });
    expect(plan).toEqual({ kind: "append", tail: [S3] });
    if (plan.kind === "append") expect(plan.tail[0]).toBe(S3);
  });

  it("is `none` when nothing is past heldMaxSeq — even if `total` jumped", () => {
    // The old count-guess would resync here on a `total` delta; seq proves the
    // newest message is already on screen.
    expect(planPage([S1, S2], [S1, S2], { knownTotal: 2, total: 60 })).toEqual({ kind: "none" });
  });

  it("is `none` for a re-parse of the same page (no reference churn)", () => {
    expect(planPage([S1, S2], [S1, S2], { knownTotal: 2, total: 2 })).toEqual({ kind: "none" });
  });

  it("fills on the first load: a whole page is appended with no hole", () => {
    const plan = planPage([], [S1, S2, S3], { knownTotal: null, total: 3 });
    expect(plan).toEqual({ kind: "append", tail: [S1, S2, S3] });
  });

  it("resyncs on a hole and counts unseen from ackedSeq, not the page length", () => {
    // held up to seq 2, but the page starts at seq 5 — seq 3/4 are missing.
    const page = [s(5, "five"), s(6, "six")];
    const plan = planPage([S1, S2], page, { knownTotal: 6, total: 6 }, 2);
    expect(plan).toEqual({ kind: "resync", messages: page, unseen: 2 });
  });

  it("treats the held-empty case as gapless regardless of page start", () => {
    // A page that does not begin at seq 1 is normal once the window rolled;
    // with nothing held there is no gap to distrust.
    const page = [S3, S4];
    expect(planPage([], page, { knownTotal: null, total: 4 })).toEqual({
      kind: "append",
      tail: page,
    });
  });
});

describe("push channel transition table (ADR-092 decision 6)", () => {
  afterEach(() => {
    vi.useRealTimers();
    setChatPushChannel(false);
  });

  it("down→up fires exactly one wake, up→up fires none, up→down re-arms the poll", async () => {
    vi.useRealTimers();
    useAuthStore.setState({ mode: "local", account: null });
    setChatPushChannel(false);
    vi.useFakeTimers();
    const srv = servePages([{ messages: [M1], total: 1 }]);
    useAuthStore.setState({
      mode: "multi_user",
      status: "logged_in",
      account: self,
      accessToken: "at",
    });

    // down → up: the edge itself must drive the catch-up re-read, and only once.
    srv.spy.mockClear();
    setChatPushChannel(true);
    await vi.advanceTimersByTimeAsync(0);
    expect(srv.chatReads()).toBe(1);

    // up → up: idempotent, no second re-read.
    srv.spy.mockClear();
    setChatPushChannel(true);
    await vi.advanceTimersByTimeAsync(0);
    expect(srv.chatReads()).toBe(0);

    // up → down: stands the doorbell path down and re-arms the timer, which
    // then reads on its own cadence.
    srv.spy.mockClear();
    setChatPushChannel(false);
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(srv.chatReads()).toBeGreaterThanOrEqual(1);
  });
});

/**
 * Drive `GET …/messages` from a queue of pages (the last one repeats), plus a
 * `/chats` route that always answers. Each read takes the next entry, so a test
 * describes the conversation the way the server would experience it.
 *
 * `echo` is what `POST …/messages` returns — the Gateway's own record of the
 * message we sent, which is also the newest entry of the next page.
 */
function servePages(
  pages: Array<{ messages: Msg[]; total: number | null }>,
  echo?: Msg,
) {
  let served = 0;
  let fail = false;
  const routes: Route[] = [
    {
      suffix: "/messages",
      method: "GET",
      make: () => {
        if (fail) throw new Error("gateway is restarting");
        const p = pages[Math.min(served, pages.length - 1)];
        served += 1;
        return json(pageBody(p.messages, p.total));
      },
    },
    { suffix: "/chats", make: () => json({ chats: [summary(0)] }) },
  ];
  if (echo) {
    // BEFORE the `/chats` route: the messages URL contains `/chats/{chat_id}/`,
    // so a route list that reaches the chat-list stub first would answer the
    // POST with a chat summary and the echo would arrive with no `body`.
    routes.splice(1, 0, {
      suffix: "/messages",
      method: "POST",
      make: () => json(echo, 201),
    });
  }
  const spy = stubFetch(routes);
  return {
    spy,
    /** Simulate the endpoint going down (a dropped re-read must not blank the thread). */
    setFail(v: boolean) {
      fail = v;
    },
    pageReads: () =>
      spy.mock.calls.filter(([u]) => String(u).includes("/messages?")).length,
    chatReads: () => spy.mock.calls.filter(([u]) => String(u).endsWith("/chats")).length,
  };
}

describe("signal-driven reload", () => {
  it("appends the arrival and leaves the already-held objects alone", async () => {
    servePages([
      { messages: [M1, M2], total: 2 },
      { messages: [M1, M2, M3], total: 3 },
    ]);
    await useUserChatStore.getState().openChat(PEER);
    const before = useUserChatStore.getState().messages;
    expect(before.map((x) => x.body)).toEqual(["one", "two"]);

    await useUserChatStore.getState().wake();

    const s = useUserChatStore.getState();
    expect(s.messages.map((x) => x.body)).toEqual(["one", "two", "three"]);
    // The reader's position rests on these two references not changing.
    expect(s.messages[0]).toBe(before[0]);
    expect(s.messages[1]).toBe(before[1]);
    expect(s.messagesTotal).toBe(3);
    // Counted as unseen; the panel clears it in the same tick iff they were at
    // the bottom already (see `InboxPanel`'s pinned gate).
    expect(s.unseenCount).toBe(1);
  });

  it("touches no state when the signal turned out to be about another thread", async () => {
    // The signal is content-free, so this is the common case, not an edge one:
    // a change in someone else's conversation still re-reads this window.
    servePages([
      { messages: [M1, M2], total: 2 },
      { messages: [M1, M2], total: 2 },
    ]);
    await useUserChatStore.getState().openChat(PEER);
    const before = useUserChatStore.getState();

    await useUserChatStore.getState().wake();

    const s = useUserChatStore.getState();
    expect(s.messages).toBe(before.messages); // not a copy: no re-render, no scroll
    expect(s.messagesTotal).toBe(2);
    expect(s.unseenCount).toBe(0);
    expect(s.loadingMessages).toBe(false);
  });

  it("resyncs a gap larger than a page and counts the rebuild honestly", async () => {
    const page = [M1, M2, M3, M4];
    servePages([
      { messages: [M1, M2], total: 2 },
      { messages: page, total: 60 },
    ]);
    await useUserChatStore.getState().openChat(PEER);

    await useUserChatStore.getState().wake();

    const s = useUserChatStore.getState();
    expect(s.messages).toBe(page);
    expect(s.messagesTotal).toBe(60);
    // No "how many did I miss" survives a rebuilt window; the count is the
    // delta the server reported, not the page length.
    expect(s.unseenCount).toBe(58);
  });

  it("treats a page with no count as a replacement, never as a delta", async () => {
    // The regression this pins: without the normalization in `listMessages`
    // the missing `total` reached the rule as NaN, every comparison fell
    // through and the whole page was appended to itself — a duplicated
    // transcript on every signal, against an older Gateway.
    servePages([
      { messages: [M1, M2], total: 2 },
      { messages: [M1, M2, M3], total: null },
    ]);
    await useUserChatStore.getState().openChat(PEER);

    await useUserChatStore.getState().wake();

    const s = useUserChatStore.getState();
    expect(s.messages.map((x) => x.body)).toEqual(["one", "two", "three"]);
    expect(s.messagesTotal).toBeNull();
  });

  it("keeps showing the window when a re-read fails, and recovers on the next", async () => {
    const srv = servePages([
      { messages: [M1, M2], total: 2 },
      { messages: [M1, M2, M3], total: 3 },
    ]);
    await useUserChatStore.getState().openChat(PEER);
    const before = useUserChatStore.getState().messages;

    srv.setFail(true);
    await useUserChatStore.getState().wake();
    expect(useUserChatStore.getState().messages).toBe(before); // not blanked
    expect(useUserChatStore.getState().error).toContain("restarting");

    srv.setFail(false);
    await useUserChatStore.getState().wake();
    const s = useUserChatStore.getState();
    expect(s.messages.map((x) => x.body)).toEqual(["one", "two", "three"]);
    expect(s.error).toBeNull();
  });

  it("counts the sent message, so its own signal does not append it twice", async () => {
    const sent = m(3, "hello", SELF);
    // The second page is the server's view after our own POST: it already
    // contains the echo, and `total` counts it.
    const srv = servePages(
      [
        { messages: [M1, M2], total: 2 },
        { messages: [M1, M2, sent], total: 3 },
      ],
      sent,
    );

    await useUserChatStore.getState().openChat(PEER);
    expect(await useUserChatStore.getState().send("hello")).toBe(true);

    const afterSend = useUserChatStore.getState();
    expect(afterSend.messages.map((x) => x.body)).toEqual(["one", "two", "hello"]);
    expect(afterSend.messagesTotal).toBe(3); // 2 held + the echo we just took

    // The doorbell our own POST caused.
    await useUserChatStore.getState().wake();

    const s = useUserChatStore.getState();
    expect(s.messages.map((x) => x.body)).toEqual(["one", "two", "hello"]);
    expect(s.unseenCount).toBe(0);
    // Two reads for the send itself (the store's own list refresh) plus the
    // wake — and never a third copy of the message.
    expect(s.messages.filter((x) => x.body === "hello")).toHaveLength(1);
    expect(srv.pageReads()).toBe(2);
  });

  it("never lets another thread's count travel with the window", async () => {
    // `messagesTotal` is per-thread: carrying it over would make the first page
    // of the next conversation look like a negative delta and force a resync.
    servePages([{ messages: [M1, M2], total: 2 }]);
    await useUserChatStore.getState().openChat(PEER);
    expect(useUserChatStore.getState().messagesTotal).toBe(2);

    await useUserChatStore.getState().openChat(
      "cccccccc-0000-4000-8000-000000000003",
    );
    expect(useUserChatStore.getState().messagesTotal).toBe(2); // re-read, not inherited
    expect(useUserChatStore.getState().unseenCount).toBe(0);

    useUserChatStore.getState().closeChat();
    expect(useUserChatStore.getState().messagesTotal).toBeNull();
  });
});

describe("wake() coalescing", () => {
  it("folds a burst of signals into one follow-up round", async () => {
    const srv = servePages([
      { messages: [M1], total: 1 },
      { messages: [M1, M2], total: 2 },
    ]);
    await useUserChatStore.getState().openChat(PEER);
    srv.spy.mockClear();

    // One send from another device emits two signals (message + read cursor),
    // so a burst is the normal case. Three rounds of reads would be three.
    await Promise.all([
      useUserChatStore.getState().wake(),
      useUserChatStore.getState().wake(),
      useUserChatStore.getState().wake(),
    ]);

    expect(srv.chatReads()).toBe(2);
    expect(srv.pageReads()).toBe(2);
    expect(useUserChatStore.getState().messages.map((x) => x.body)).toEqual(["one", "two"]);
  });

  it("leaves the merge state clean, so the next signal is not swallowed", async () => {
    const srv = servePages([
      { messages: [M1], total: 1 },
      { messages: [M1, M2], total: 2 },
      { messages: [M1, M2, M3], total: 3 },
    ]);
    await useUserChatStore.getState().openChat(PEER);

    await Promise.all([
      useUserChatStore.getState().wake(),
      useUserChatStore.getState().wake(),
    ]);
    await useUserChatStore.getState().wake();

    expect(useUserChatStore.getState().messages.map((x) => x.body)).toEqual([
      "one",
      "two",
      "three",
    ]);
  });

  it("reads the inbox even with no thread open", async () => {
    // A first message in a brand-new conversation is only discoverable through
    // the chat list, which is why `wake` never skips that half.
    const srv = servePages([{ messages: [M1], total: 1 }]);
    srv.spy.mockClear();

    await useUserChatStore.getState().wake();

    expect(srv.chatReads()).toBe(1);
    expect(srv.pageReads()).toBe(0);
  });
});

describe("push channel vs fallback poll", () => {
  afterEach(() => {
    vi.useRealTimers();
    setChatPushChannel(false);
  });

  it("stands the timer down when the bridge comes up, and re-arms when it drops", async () => {
    vi.useRealTimers();
    useAuthStore.setState({ mode: "local", account: null });
    setChatPushChannel(false);
    vi.useFakeTimers();
    const srv = servePages([{ messages: [M1], total: 1 }]);

    useAuthStore.setState({
      mode: "multi_user",
      status: "logged_in",
      account: self,
      accessToken: "at",
    });
    const armed = srv.chatReads();
    expect(armed).toBeGreaterThan(0);
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(srv.chatReads()).toBe(armed + 1);

    // The MQTT bridge came up. This is the trap that the separation of `reset`
    // from `syncPollLoop` guards: standing the poll down must not look like a
    // logout, or the inbox empties the moment MQTT connects.
    setChatPushChannel(true);
    expect(useUserChatStore.getState().chats).toHaveLength(1);
    const stopped = srv.chatReads();
    await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    expect(srv.chatReads()).toBe(stopped);

    // And the degraded path still converges: a bridge that drops re-arms.
    setChatPushChannel(false);
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(srv.chatReads()).toBeGreaterThan(stopped);
  });

  it("does not arm for a session that signs in while the bridge is already up", async () => {
    vi.useRealTimers();
    useAuthStore.setState({ mode: "local", account: null });
    vi.useFakeTimers();
    const srv = servePages([{ messages: [M1], total: 1 }]);

    setChatPushChannel(true);
    useAuthStore.setState({
      mode: "multi_user",
      status: "logged_in",
      account: self,
      accessToken: "at",
    });
    srv.spy.mockClear();
    await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    // Push owns freshness while it is up — including the immediate read a
    // sign-in would otherwise have made.
    expect(srv.chatReads()).toBe(0);
  });

  it("still resets the inbox when the account actually changes hands", async () => {
    // The other half of the split: `setChatPushChannel` must not be the only
    // thing that can leave state behind.
    servePages([{ messages: [M1, M2], total: 2 }]);
    await useUserChatStore.getState().openChat(PEER);
    expect(useUserChatStore.getState().messages).toHaveLength(2);

    const other: UserAccount = { ...self, user_id: PEER, username: "alice" };
    useAuthStore.setState({
      mode: "multi_user",
      status: "logged_in",
      account: other,
      accessToken: "at2",
    });

    const s = useUserChatStore.getState();
    expect(s.messages).toHaveLength(0);
    expect(s.messagesTotal).toBeNull();
    expect(s.activePeerId).toBeNull();
  });
});
