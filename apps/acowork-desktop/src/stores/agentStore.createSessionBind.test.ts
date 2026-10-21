/**
 * ADR-099: the Chat surface's `+` creates a NEW TASK that belongs to the
 * conversation. `agentStore.createSession(agentId, { bindToChat: true })`
 * carries the agent's Chat session id so the Runtime stamps the
 * `chat_session_id` + `Members` pair — the same one `dispatch(new)` stamps —
 * and the fresh session lands in the Chat Router's candidate list.
 *
 * The three rules pinned here are all about *when* the id is sent:
 *  - only for the user-pressed entry points (`bindToChat`), never for the
 *    automatic creations (first open, closing the last exec tab), which stay
 *    unbound so they cannot quietly join a conversation's candidate list;
 *  - the id comes from the store's mirror of the server row, so an agent with
 *    no resolved Chat session sends none (local mode / legacy exec surface);
 *  - the frontend never invents a binding: it forwards, and the Runtime
 *    re-checks the caller's chat membership (see `may_bind_chat`).
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

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

import { useAgentStore } from "./agentStore";
import { useChatSessionStore } from "./chatSessionStore";

const AGENT = "com.test.bind_to_chat";
const CHAT = "cs-1";

interface Call {
  url: string;
  body: Record<string, unknown>;
}

let calls: Call[] = [];

beforeEach(() => {
  calls = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
      calls.push({
        url: String(url),
        body: init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {},
      });
      return {
        ok: true,
        status: 201,
        json: async () => ({ session_id: "s-new" }),
        text: async () => "",
        headers: { get: () => null },
      };
    }),
  );
  // A clean slate for both stores: no chat resolution cached, no workspace.
  useChatSessionStore.setState({ byAgent: {} });
});

function resolveChat(chatSessionId: string | null) {
  useChatSessionStore.setState((s) => ({
    byAgent: {
      ...s.byAgent,
      [AGENT]: { chatSessionId, resolved: true, mode: "chat" },
    },
  }));
}

/** The `POST …/sessions` body of the single call made. */
function sessionPost(): Call {
  const post = calls.find((c) => c.url.endsWith(`/api/agents/${AGENT}/sessions`));
  if (!post) throw new Error(`no session POST among ${JSON.stringify(calls)}`);
  return post;
}

describe("agentStore.createSession chat binding", () => {
  it("sends chat_session_id when the Chat surface's + is pressed", async () => {
    resolveChat(CHAT);
    await useAgentStore.getState().createSession(AGENT, { bindToChat: true });

    expect(sessionPost().body.chat_session_id).toBe(CHAT);
  });

  it("sends nothing when this agent has no resolved Chat session", async () => {
    resolveChat(null);
    await useAgentStore.getState().createSession(AGENT, { bindToChat: true });

    expect("chat_session_id" in sessionPost().body).toBe(false);
  });

  it("stays unbound on the automatic creations (no bindToChat)", async () => {
    // Same agent, chat resolved — the caller just did not ask for a binding:
    // `resolveActiveSession` and the close-last-tab fallback must not slip an
    // unrelated session into the conversation's candidate list.
    resolveChat(CHAT);
    await useAgentStore.getState().createSession(AGENT);

    expect("chat_session_id" in sessionPost().body).toBe(false);
  });
});
