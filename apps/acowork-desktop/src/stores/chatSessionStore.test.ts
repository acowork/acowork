/**
 * The desktop Chat-session surface (design 27 §6, plan Wave 5).
 *
 * The chat window is a durable EVENT LOG — there is no read-time turn
 * projection any more (that lived to serve the removed status card). What
 * the store still owns and these tests pin:
 *
 *   1. the durable card projections — `closedCardIds` / `humanAnsweredCardIds`
 *      decide when an `interaction_request` can no longer be answered and
 *      whether it was answered by a human (§5.5.4);
 *   2. the freshness loop — the `chat/changed` doorbell is content-free, so
 *      `wake()` re-reads over HTTP and never patches a row from a signal;
 *      `sendText` re-reads once after the POST because the durable row
 *      exists before the delivery resolves (§5.1).
 *
 * `fetch` is the seam (the store talks to the Gateway's user proxy
 * directly); local mode (no account) must fall the store to mode 'exec'.
 * The pair has EXACTLY ONE Chat session (WeChat model): `openAgent`
 * resolves it through the default endpoint and never fetches a session
 * list.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import {
  closedCardIds,
  humanAnsweredCardIds,
  useChatSessionStore,
  wakeAgentChat,
  type ChatMessageRow,
  type ChatSessionConfig,
} from "./chatSessionStore";
import { useAuthStore } from "./authStore";

const PAGE = { messages: [] as ChatMessageRow[], total: 0 };

// A shared default router: the GET default session, the intent POST, the
// message window. Draft tests override it per-case via mockImplementation and
// reset to this in `beforeEach`. `init` is read so a PATCH body can be asserted.
async function defaultFetch(url: string, init?: RequestInit): Promise<unknown> {
  if (init?.method === "PATCH" && url.includes("/session-config")) {
    return { ok: false, status: 503, json: async () => ({}), text: async () => "" };
  }
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
  if (url.includes("/api/intents/chat-message")) return { ok: true, json: async () => ({}) };
  if (url.includes("/api/intents/mentions")) return { ok: true, json: async () => ({}) };
  if (url.includes("/messages")) return { ok: true, json: async () => ({ ...PAGE }) };
  if (url.includes("/api/chat-sessions/u1")) {
    return { ok: true, json: async () => [] };
  }
  return { ok: false, status: 503, json: async () => ({}), text: async () => "" };
}
const fetchMock = vi.fn(defaultFetch);
vi.stubGlobal("fetch", fetchMock);

const calls = (needle: string) =>
  fetchMock.mock.calls.filter((c) => String(c[0]).includes(needle));

const row = (kind: string, meta: Partial<NonNullable<ChatMessageRow["meta"]>> = {}): ChatMessageRow => ({
  seq: 0,
  ts: 0,
  from: "u1",
  kind,
  body: "x",
  to: "",
  meta: { message_id: "", ...meta },
});

const user = (id: string) => row("user_message", { message_id: id });
const pointer = (trigger: string, sid: string) =>
  row("system_notice", { notice_type: "turn_started", in_reply_to: trigger, agent_session_id: sid });
const card = (id: string, trigger: string, sid?: string) =>
  row("interaction_request", {
    message_id: id,
    in_reply_to: trigger,
    ...(sid ? { agent_session_id: sid } : {}),
  });

describe("closedCardIds / humanAnsweredCardIds (design 27 §5.5.4 — card self-heal)", () => {
  it("closes a card once its turn writes a terminal agent_message", () => {
    const rows = [
      user("t1"),
      pointer("t1", "exec-9"),
      card("c1", "t1", "exec-9"),
      row("agent_message", { message_id: "resp-t1", in_reply_to: "t1", sender_session_id: "exec-9" }),
    ];
    expect(closedCardIds(rows).has("c1")).toBe(true);
  });

  it("keeps a card open while its turn ended `waiting` (the pause outcome)", () => {
    const rows = [
      user("t1"),
      pointer("t1", "exec-9"),
      card("c1", "t1", "exec-9"),
      row("agent_message", { message_id: "resp-t1", in_reply_to: "t1", sender_session_id: "exec-9", status: "waiting" }),
    ];
    expect(closedCardIds(rows).has("c1")).toBe(false);
  });

  it("a terminal from a DIFFERENT exec session does not close this agent's card (group fan-out)", () => {
    const rows = [
      user("t1"),
      pointer("t1", "exec-9"),
      card("c1", "t1", "exec-9"),
      row("agent_message", { message_id: "resp-x", in_reply_to: "t1", sender_session_id: "exec-other" }),
    ];
    expect(closedCardIds(rows).has("c1")).toBe(false);
  });

  it("a human answer closes the card and counts as answered", () => {
    const rows = [user("t1"), card("c1", "t1"), row("user_message", { message_id: "t2", in_reply_to: "c1" })];
    expect(closedCardIds(rows).has("c1")).toBe(true);
    expect(humanAnsweredCardIds(rows).has("c1")).toBe(true);
  });
});

describe("chatSessionStore freshness loop", () => {
  beforeEach(() => {
    useChatSessionStore.setState({ byAgent: {}, windows: {}, sending: {}, draft: {} });
    fetchMock.mockClear();
    fetchMock.mockImplementation(defaultFetch);
    useAuthStore.setState({ account: { user_id: "u1" } as never, accessToken: "tok" });
  });

  it("openAgent resolves the single default session and loads its window", async () => {
    await useChatSessionStore.getState().openAgent("agent-1");
    const st = useChatSessionStore.getState().byAgent["agent-1"];
    expect(st.chatSessionId).toBe("cs-1");
    expect(st.resolved).toBe(true);
    expect(st.mode).toBe("chat");
    expect(calls("/messages").length).toBe(1);
    // One pair, one chat session: no per-agent session list is fetched.
    expect(calls("?agent=").length).toBe(0);
  });

  it("no account (local mode) resolves straight to the exec surface — the rollback path", async () => {
    useAuthStore.setState({ account: null, accessToken: null });
    await useChatSessionStore.getState().openAgent("agent-1");
    expect(useChatSessionStore.getState().byAgent["agent-1"]).toEqual({
      chatSessionId: null,
      resolved: true,
      mode: "exec",
    });
    expect(fetchMock.mock.calls.length).toBe(0);
  });

  it("openAgent failure resolves to the exec surface, never a stuck spinner", async () => {
    fetchMock.mockImplementationOnce(async () => ({ ok: false, status: 503, json: async () => ({}), text: async () => "" }));
    await useChatSessionStore.getState().openAgent("agent-dead");
    expect(useChatSessionStore.getState().byAgent["agent-dead"].mode).toBe("exec");
    expect(useChatSessionStore.getState().byAgent["agent-dead"].resolved).toBe(true);
  });

  it("sendText posts through the intent channel and re-reads once (§5.1)", async () => {
    await useChatSessionStore.getState().openAgent("agent-1");
    fetchMock.mockClear();
    const ok = await useChatSessionStore.getState().sendText("cs-1", "hello", "card-9");
    expect(ok).toBe(true);
    const post = calls("/api/intents/chat-message");
    expect(post.length).toBe(1);
    const body = JSON.parse((post[0]![1] as RequestInit).body as string);
    expect(body).toMatchObject({ chat_session_id: "cs-1", from_user: "u1", text: "hello", in_reply_to: "card-9" });
    expect(calls("/messages").length).toBe(1);
  });

  it("wake() re-reads the pinned chat session over HTTP even in exec mode — the tab's pulse must stay honest", async () => {
    await useChatSessionStore.getState().openAgent("agent-1");
    useChatSessionStore.getState().setMode("agent-1", "exec");
    fetchMock.mockClear();
    wakeAgentChat();
    await vi.waitFor(() => expect(calls("/messages").length).toBe(1));
    expect(useChatSessionStore.getState().windows["cs-1"]!.tail).toEqual([]);
  });
});

describe("chatSessionStore seed draft (design 27 §4.5 — backend-persisted, store is a cache)", () => {
  beforeEach(() => {
    useChatSessionStore.setState({ byAgent: {}, windows: {}, sending: {}, draft: {} });
    fetchMock.mockClear();
    fetchMock.mockImplementation(defaultFetch);
    useAuthStore.setState({ account: { user_id: "u1" } as never, accessToken: "tok" });
  });

  it("hydrates the draft cache from the default session's session_config", async () => {
    fetchMock.mockImplementation(async (url: string) => {
      if (String(url).includes("/api/chat-sessions/default")) {
        return {
          ok: true,
          json: async () => ({
            session_id: "cs-1",
            kind: "user_agent",
            title: "",
            session_config: { model: "gpt-x", provider: "prov-y" },
            created_at: 1,
            last_active_at: 1,
            message_count: 0,
          }),
        };
      }
      if (String(url).includes("/messages")) return { ok: true, json: async () => ({ messages: [], total: 0 }) };
      return { ok: false, status: 503, json: async () => ({}), text: async () => "" };
    });
    await useChatSessionStore.getState().openAgent("agent-1");
    expect(useChatSessionStore.getState().draft["cs-1"]).toEqual({ model: "gpt-x", provider: "prov-y" });
  });

  it("saveDraft PATCHes the whole draft (undefined/empty stripped), shows it optimistically, then adopts the server-normalized value", async () => {
    await useChatSessionStore.getState().openAgent("agent-1"); // default carries no session_config → {}
    fetchMock.mockClear();
    let optimistic: ChatSessionConfig | undefined;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (init?.method === "PATCH") {
        // Mid-flight the cache already reflects the full pick (optimistic UI).
        optimistic = useChatSessionStore.getState().draft["cs-1"];
        return {
          ok: true,
          json: async () => ({
            session_id: "cs-1",
            kind: "user_agent",
            title: "",
            session_config: { model: "m2", provider: "p2" },
            created_at: 1,
            last_active_at: 1,
            message_count: 0,
          }),
        };
      }
      return defaultFetch(url, init);
    });
    await useChatSessionStore
      .getState()
      .saveDraft("agent-1", { model: "m2", provider: "p2", workspace: undefined, reasoning_effort: undefined });
    const patch = calls("/session-config");
    expect(patch.length).toBe(1);
    const body = JSON.parse((patch[0]![1] as RequestInit).body as string);
    // Whole-replace wire strips undefined/empty — an absent field clears server-side.
    expect(body).toEqual({ model: "m2", provider: "p2" });
    expect(optimistic).toEqual({ model: "m2", provider: "p2" });
    // Final cache is the server-normalized row, not the local guess.
    expect(useChatSessionStore.getState().draft["cs-1"]).toEqual({ model: "m2", provider: "p2" });
  });

  it("reverts the optimistic draft when the PATCH fails — a draft that never landed must not be shown", async () => {
    await useChatSessionStore.getState().openAgent("agent-1");
    useChatSessionStore.setState({ draft: { "cs-1": { model: "keep" } } });
    fetchMock.mockClear();
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (init?.method === "PATCH") return { ok: false, status: 503, json: async () => ({}), text: async () => "" };
      return defaultFetch(url, init);
    });
    await useChatSessionStore.getState().saveDraft("agent-1", { model: "gone" });
    expect(useChatSessionStore.getState().draft["cs-1"]).toEqual({ model: "keep" });
  });
});

describe("sendGroupText attachment passthrough (design 27 §4.2.1 / §5.3)", () => {
  beforeEach(() => {
    useChatSessionStore.setState({ byAgent: {}, windows: {}, sending: {}, draft: {} });
    fetchMock.mockClear();
    fetchMock.mockImplementation(defaultFetch);
    useAuthStore.setState({ account: { user_id: "u1" } as never, accessToken: "tok" });
  });

  it("@-fan-out forwards attachment_ids on the mentions request", async () => {
    const ok = await useChatSessionStore
      .getState()
      .sendGroupText("grp-1", "@Bob see this", ["bob"], undefined, ["att-1", "att-2"]);
    expect(ok).toBe(true);
    const post = calls("/api/intents/mentions");
    expect(post.length).toBe(1);
    const body = JSON.parse((post[0]![1] as RequestInit).body as string);
    expect(body).toMatchObject({
      chat_session_id: "grp-1",
      from_user: "u1",
      targets: ["bob"],
      text: "@Bob see this",
      attachment_ids: ["att-1", "att-2"],
    });
  });

  it("archive-only group message forwards attachment_ids on the row write", async () => {
    const ok = await useChatSessionStore
      .getState()
      .sendGroupText("grp-1", "just a pic", [], undefined, ["att-1"]);
    expect(ok).toBe(true);
    // The write POSTs to `/messages`; the §5.1 re-read GETs the same path,
    // so pin the POST by method.
    const post = calls("/messages").filter((c) => (c[1] as RequestInit)?.method === "POST");
    expect(post.length).toBe(1);
    const body = JSON.parse((post[0]![1] as RequestInit).body as string);
    expect(body).toMatchObject({ body: "just a pic", to: "", attachment_ids: ["att-1"] });
  });

  it("an attachment-only mention sends with empty text", async () => {
    const ok = await useChatSessionStore
      .getState()
      .sendGroupText("grp-1", "", ["bob"], undefined, ["att-1"]);
    expect(ok).toBe(true);
    const body = JSON.parse((calls("/api/intents/mentions")[0]![1] as RequestInit).body as string);
    expect(body).toMatchObject({ text: "", attachment_ids: ["att-1"] });
  });

  it("a card answer never carries attachments — the send is refused", async () => {
    const ok = await useChatSessionStore
      .getState()
      .sendGroupText("grp-1", "answer", [], "card-9", ["att-1"]);
    expect(ok).toBe(false);
    // Neither the chat-message channel nor the mention channel fired.
    expect(calls("/api/intents/chat-message").length).toBe(0);
    expect(calls("/api/intents/mentions").length).toBe(0);
  });
});
