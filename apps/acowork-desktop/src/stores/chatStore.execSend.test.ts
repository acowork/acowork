/**
 * The execution composer writes into its OWN session — plus one mirror row.
 *
 * A message typed in an execution view is a session-scoped write
 * (`POST /api/agents/{id}/sessions/{sid}/messages`). It is never delivered
 * through the Chat channel (`/api/intents/chat-message`) and wakes no router
 * turn, even when the session is bound to a Chat: the old full mirror is
 * what forced a delivery pin (`for_execution_session`), and dropping that
 * mirror removed the pin and with it the only path by which an execution
 * utterance could reach the Chat Router's create branch.
 *
 * ADR-106's revision of 2026-11-16 restores exactly ONE row of the mirror,
 * without restoring the delivery: the FIRST plain-text message of a
 * chat-bound session is appended to that Chat as an archive-only
 * `user_message` (`POST /api/chat-sessions/{id}/messages`, keyed by the same
 * `message_id` the session write carries, so a retry cannot double it), and
 * the session write then carries `chat_trigger_row_id`. The Runtime honors
 * the claim only for the first round of a chat-bound session; every
 * degradation — unbound, later round, failed append, command turn — leaves
 * the turn exactly as the pre-revision shape: unthreaded, no second row.
 *
 * The invariants pinned here:
 *  - bound or unbound, signed-in or local mode, the session write is the path;
 *  - `/api/intents/chat-message` is never touched from an execution view;
 *  - the turn facts the session write carries (`command`, `attached_items`
 *    inside `params_json`) still travel, so the deletion is not a capability
 *    regression;
 *  - the request names the id of the bubble already painted, so the server
 *    copy reconciles instead of duplicating;
 *  - the mirror is one row for the opener only, is archive-only, and its
 *    failure never blocks the turn.
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
    // The delivery never rides the Chat channel: the Router is not consulted
    // and no turn is started by this send. The opener's mirror row goes to
    // the archive endpoint instead (pinned in its own block below), which is
    // a write with no delivery at all.
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

describe("the opener mirror (ADR-106 revision 2026-11-16)", () => {
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

  const mirror = () => sentTo(`/chat-sessions/${CHAT}/messages`);

  it("appends the first words to the bound Chat and claims the thread", async () => {
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("fix the pagination", AGENT);

    const m = mirror();
    expect(m).toBeTruthy();
    const bubble = useChatStore
      .getState()
      .agentStates[AGENT]?.sessionStates[SESSION]?.messages.find((x) => x.type === "user");
    // One id, two transcripts: the mirror row is keyed by the very
    // `message_id` the session write carries, so the Runtime's round key and
    // the bubble coordinate name the same human message, and a retry of the
    // pair dedups on the store side.
    expect(m!.body).toMatchObject({
      body: "fix the pagination",
      message_id: bubble!.id,
    });
    expect(sessionBody().chat_trigger_row_id).toBe(bubble!.id);
    // The mirror lands BEFORE the session write, so the reference row the
    // Runtime publishes at the round boundary can never point above a row
    // that does not exist yet (the store resolves `in_reply_to`).
    const mi = calls.findIndex((c) => c === m);
    const si = calls.findIndex((c) => c === sentTo(`/sessions/${SESSION}/messages`));
    expect(mi).toBeLessThan(si);
  });

  it("mirrors nothing for a session with no chat binding", async () => {
    seedSession(null);
    await useChatStore.getState().sendMessage("hello", AGENT);

    expect(mirror()).toBeUndefined();
    expect(sessionBody().chat_trigger_row_id).toBeUndefined();
  });

  it("mirrors nothing after the first message", async () => {
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("first words", AGENT);
    const firstMirrorCount = calls.filter((c) =>
      c.url.endsWith(`/chat-sessions/${CHAT}/messages`),
    ).length;
    expect(firstMirrorCount).toBe(1);

    await useChatStore.getState().sendMessage("follow-up words", AGENT);
    // The follow-up stays the session's own working transcript; the Chat
    // gains no row for it (Rev. 8 stands for everything after the opener).
    expect(
      calls.filter((c) => c.url.endsWith(`/chat-sessions/${CHAT}/messages`)).length,
    ).toBe(firstMirrorCount);
    const lastSend = [...calls]
      .reverse()
      .find((c) => c.method === "POST" && c.url.endsWith(`/sessions/${SESSION}/messages`));
    expect(lastSend!.body.chat_trigger_row_id).toBeUndefined();
  });

  it("runs the turn unthreaded when the mirror append fails", async () => {
    seedSession(CHAT);
    const real = fetch;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
        if (String(url).endsWith(`/chat-sessions/${CHAT}/messages`)) {
          return { ok: false, status: 503, json: async () => ({}), text: async () => "down" };
        }
        return real(url as RequestInfo, init);
      }),
    );

    await useChatStore.getState().sendMessage("degrade honestly", AGENT);

    // No mirror row, no claim: the pre-revision shape, and the turn still ran.
    expect(sessionBody().chat_trigger_row_id).toBeUndefined();
    expect(sentTo(`/sessions/${SESSION}/messages`)).toBeTruthy();
  });

  it("mirrors nothing for a command turn or an empty body", async () => {
    seedSession(CHAT);
    await useChatStore.getState().sendMessage("run the skill", AGENT, "/review");
    expect(mirror()).toBeUndefined();
    expect(sessionBody().chat_trigger_row_id).toBeUndefined();
  });
});
