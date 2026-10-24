/**
 * ADR-100 D12 — the execution composer sends through the Chat entry point when
 * its session came from a Chat.
 *
 * The rule pinned here is about *which* endpoint a send lands on, because that
 * is what decides whether the conversation has a real human turn in it:
 *  - a session bound to a Chat posts to `/api/intents/chat-message`, pinned
 *    back to itself (`for_execution_session`), so the durable `user_message`
 *    row is written under the caller's identity and the Agent skips its router;
 *  - an unbound session (or local mode, where there is no Chat surface at all)
 *    keeps the session-scoped write, which is its only path;
 *  - both paths send the SAME client-minted `message_id`, so the optimistic
 *    bubble reconciles with the chat row and with the execution session's own
 *    JSONL row instead of duplicating.
 *
 * The turn facts the chat channel did not carry before (`command`,
 * `attached_items`) ride the pinned request as structured fields, so a pinned
 * send is not a capability regression against the session-scoped one.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

vi.mock("../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
  setLevel: () => {},
  getLevel: () => "off" as const,
}));

// ── SUT ──────────────────────────────────────────────────────────────────

import { useChatStore } from "./chatStore";
import { useAgentStore } from "./agentStore";
import { useAuthStore } from "./authStore";
import type { AttachedItem } from "../lib/types";

const AGENT = "com.test.pinned_send";
const SESSION = "sess-exec-1";
const CHAT = "cs-1";
const ME = "u-me";

interface Call {
  url: string;
  method: string;
  body: Record<string, unknown>;
}

let calls: Call[];

/** The messages endpoint answers an empty tail so the post-send reconcile,
 *  the interactions touch and every other incidental fetch are inert. */
function stubFetch() {
  calls = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
      calls.push({
        url: String(url),
        method: (init?.method ?? "GET").toUpperCase(),
        body: init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {},
      });
      return {
        ok: true,
        status: 200,
        json: async () => ({ messages: [], offset: 0, limit: 0, total: 0 }),
        text: async () => "",
        headers: { get: () => null },
      };
    }),
  );
}

function sentTo(suffix: string): Call | undefined {
  return calls.find((c) => c.method === "POST" && c.url.endsWith(suffix));
}

/** Bind (or leave unbound) the ACTIVE execution session, and put it in the
 *  agent's session list the way `listSessions` would. */
function seedSession(chatSessionId?: string | null) {
  useAgentStore.setState((s) => ({
    agents: {
      ...s.agents,
      [AGENT]: {
        ...(s.agents[AGENT] ?? {}),
        sessions: [
          {
            session_id: SESSION,
            created_at: "2026-01-01T00:00:00Z",
            message_count: 0,
            title: "task",
            ...(chatSessionId === undefined ? {} : { chat_session_id: chatSessionId }),
          },
        ],
      },
    },
  }));
  useChatStore.setState((s) => {
    const agent = s.agentStates[AGENT];
    const ss = agent?.sessionStates[SESSION];
    return {
      agentStates: {
        ...s.agentStates,
        [AGENT]: {
          ...agent,
          activeSessionId: SESSION,
          sessionStates: {
            ...agent?.sessionStates,
            [SESSION]: {
              ...ss,
              messages: [],
              attachedContext: [],
              messageTotal: 0,
              messageLimit: 0,
              messageOffset: 0,
            },
          },
        },
      },
    } as never;
  });
}

const workspaceRef = {
  type: "attached_file",
  absPath: "C:\\work\\project\\src\\main.rs",
  name: "src/main.rs",
} as unknown as AttachedItem;

describe("ADR-100 D12: the execution composer routes by the Chat binding", () => {
  beforeEach(() => {
    stubFetch();
    useAuthStore.setState({ account: { user_id: ME } as never });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    useChatStore.setState({ agentStates: {} });
    useAgentStore.setState({ agents: {} });
    useAuthStore.setState({ account: null });
  });

  it("posts to the chat channel, pinned to this session, when the session is bound", async () => {
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("run the tests again", AGENT);

    const chat = sentTo("/api/intents/chat-message");
    expect(chat).toBeTruthy();
    expect(chat!.body).toMatchObject({
      chat_session_id: CHAT,
      from_user: ME,
      text: "run the tests again",
      for_execution_session: SESSION,
      // The bound chat may be a project group (a PM task is dispatched from
      // one), and a group has no single agent to deliver to — so the send has
      // to name the instance that owns this execution session.
      pinned_agent: AGENT,
    });
    // The session-scoped write is not also made — one send, one row.
    expect(sentTo(`/sessions/${SESSION}/messages`)).toBeUndefined();
  });

  it("keeps the session-scoped write when the session has no chat binding", async () => {
    // A session created outside the Chat surface (first open, close-last-tab)
    // has no conversation to file into; the pre-D12 path is still its only one.
    seedSession(null);
    await useChatStore.getState().sendMessage("hello", AGENT);

    expect(sentTo("/api/intents/chat-message")).toBeUndefined();
    expect(sentTo(`/sessions/${SESSION}/messages`)!.body).toMatchObject({
      content: "hello",
    });
  });

  it("falls back to the session-scoped write in local mode, where there is no account", async () => {
    // The chat channel is identity-bearing; without a signed-in caller the
    // Gateway 401s it. Bound metadata must not buy a guaranteed failure.
    useAuthStore.setState({ account: null });
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("hello", AGENT);

    expect(sentTo("/api/intents/chat-message")).toBeUndefined();
    expect(sentTo(`/sessions/${SESSION}/messages`)).toBeTruthy();
  });

  it("carries the skill name and the attached items the session write would have carried", async () => {
    // A pinned send must not silently lose the turn facts: `attached_items`
    // travels as structured entries (the session path sends them JSON-encoded
    // inside `params_json`, which the chat channel has no reason to reuse).
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("look at this", AGENT, "/review", [workspaceRef]);

    const chat = sentTo("/api/intents/chat-message")!;
    expect(chat.body.command).toBe("/review");
    // Forwarded exactly as the session-scoped write would have forwarded them
    // inside `params_json` — including the `clientId` the optimistic chip is
    // keyed on, which the Runtime echoes back so the chip settles.
    expect(chat.body.attached_items).toMatchObject([
      {
        type: "attached_file",
        absPath: "C:\\work\\project\\src\\main.rs",
        name: "src/main.rs",
      },
    ]);
    expect(String(JSON.stringify(chat.body.attached_items))).toContain("src/main.rs");
  });

  it("omits the empty optional fields instead of sending blanks", async () => {
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("plain words", AGENT);

    const body = sentTo("/api/intents/chat-message")!.body;
    expect("command" in body).toBe(false);
    expect("attached_items" in body).toBe(false);
  });

  it("sends the id of the bubble it already painted, so one row reconciles everywhere", async () => {
    // The optimistic insert happens before the request; the request must name
    // that same id or the server copy would land as a second message.
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("same id please", AGENT);

    const bubble = useChatStore
      .getState()
      .agentStates[AGENT]?.sessionStates[SESSION]?.messages.find((m) => m.type === "user");
    const chat = sentTo("/api/intents/chat-message")!.body;
    expect(bubble).toBeTruthy();
    expect(chat.message_id).toBe(bubble!.id);
    expect(String(chat.message_id)).toMatch(/^msg-[0-9a-f-]{36}$/);
  });
});
