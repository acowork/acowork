/**
 * The execution composer writes into its OWN session, always.
 *
 * A message typed in an execution view is a session-scoped write
 * (`POST /api/agents/{id}/sessions/{sid}/messages`). It is not posted to the
 * Chat channel, even when the session is bound to one: mirroring the human's
 * words into the Chat transcript is what forced a delivery pin
 * (`for_execution_session`) so the turn would not be re-routed into a *new*
 * execution session by the very message written to describe it. Dropping the
 * mirror removes the pin, and with it the only path by which a human utterance
 * could reach the Chat Router's create branch.
 *
 * The invariant pinned here is therefore about *which* endpoint a send lands
 * on and on nothing else:
 *  - bound or unbound, signed-in or local mode, the session write is the path;
 *  - `/api/intents/chat-message` is never touched from an execution view;
 *  - the turn facts the session write carries (`command`, `attached_items`
 *    inside `params_json`) still travel, so the deletion is not a capability
 *    regression;
 *  - the request names the id of the bubble already painted, so the server
 *    copy reconciles instead of duplicating.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

vi.mock("../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
  setLevel: () => {},
  getLevel: () => "off" as const,
}));

// ── SUT ──────────────────────────────

import { useChatStore } from "./chatStore";
import { useAgentStore } from "./agentStore";
import { useAuthStore } from "./authStore";
import type { AttachedItem } from "../lib/types";

const AGENT = "com.test.exec_send";
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

const sessionBody = () => sentTo(`/sessions/${SESSION}/messages`)!.body;

describe("the execution composer writes into its own session", () => {
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

  it("does not post to the chat channel when the session is bound", async () => {
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("run the tests again", AGENT);

    expect(sentTo(`/sessions/${SESSION}/messages`)).toBeTruthy();
    // One send, one row: the Chat transcript is not a second home for the
    // same words, so no chat-channel request is made at all.
    expect(sentTo("/api/intents/chat-message")).toBeUndefined();
  });

  it("writes to its own session when the session has no chat binding", async () => {
    // A session created outside the Chat surface (first open, close-last-tab).
    seedSession(null);
    await useChatStore.getState().sendMessage("hello", AGENT);

    expect(sentTo("/api/intents/chat-message")).toBeUndefined();
    expect(sessionBody()).toMatchObject({ content: "hello" });
  });

  it("writes to its own session in local mode, where there is no account", async () => {
    useAuthStore.setState({ account: null });
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("hello", AGENT);

    expect(sentTo("/api/intents/chat-message")).toBeUndefined();
    expect(sentTo(`/sessions/${SESSION}/messages`)).toBeTruthy();
  });

  it("carries the skill name and the attached items the turn was built with", async () => {
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("look at this", AGENT, "/review", [workspaceRef]);

    const body = sessionBody();
    expect(body.command).toBe("/review");
    // Attachments ride `params_json` as the ADR-046 union, including the
    // `clientId` the optimistic chip is keyed on.
    const params = JSON.parse(String(body.params_json));
    expect(params.attached_items).toMatchObject([
      {
        type: "attached_file",
        absPath: "C:\\work\\project\\src\\main.rs",
        name: "src/main.rs",
      },
    ]);
  });

  it("sends an empty params_json when the turn carries no files", async () => {
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("plain words", AGENT);

    const body = sessionBody();
    expect(body.params_json).toBe("");
  });

  it("sends the id of the bubble it already painted, so one row reconciles", async () => {
    // The optimistic insert happens before the request; the request must name
    // that same id or the server copy would land as a second message.
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("same id please", AGENT);

    const bubble = useChatStore
      .getState()
      .agentStates[AGENT]?.sessionStates[SESSION]?.messages.find((m) => m.type === "user");
    expect(bubble).toBeTruthy();
    expect(sessionBody().message_id).toBe(bubble!.id);
    expect(String(bubble!.id)).toMatch(/^msg-[0-9a-f-]{36}$/);
  });
});
