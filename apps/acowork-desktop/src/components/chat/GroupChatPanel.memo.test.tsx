// @vitest-environment jsdom
/**
 * Row-level memo contract of the group chat surface (chat-transcript
 * render-perf plan L1) — the twin of `AgentChatView.memo.test.tsx` for the
 * second copy of the same defect.
 *
 * A composer keystroke re-renders the host, the host re-runs `rows.map`;
 * with `GroupRowView` memoized and every host→row prop reference-stable, the
 * keystroke must produce ZERO row renders. Row renders are counted through
 * `useSessionTitle` — the one hook `GroupRowView` calls unconditionally on
 * every render and nowhere else in this tree.
 *
 * Store mocks keep the REAL app's stability contract (single action object,
 * single store-state object, stable `t`): `nameOf` is a `useCallback` over
 * `[agentsMap, dirById]`, so a harness handing a fresh `agents` literal per
 * selector call would defeat the memo by mock artifact, not by product bug.
 */
import { act, fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT = "agent-1";
const ME = "user-me";
const GROUP = "group-1";

const rowRenders = vi.hoisted(() => ({ n: 0 }));

let tail: ChatMessageRow[] = [];
let winObj: Record<string, unknown> = {};

vi.mock("@tauri-apps/api/core", () => ({ convertFileSrc: (x: string) => x }));

vi.mock("../../lib/useSessionTitle", () => ({
  useSessionTitle: () => {
    rowRenders.n += 1;
    return undefined;
  },
}));

vi.mock("../../stores/chatSessionStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../stores/chatSessionStore")>();
  const state = {
    setMode: vi.fn(),
    sendGroupText: vi.fn().mockResolvedValue(true),
    loadOlder: vi.fn().mockResolvedValue(undefined),
    loadNewer: vi.fn().mockResolvedValue(undefined),
    clearLocate: vi.fn(),
    upload: vi.fn().mockResolvedValue({ id: "a1", filename: "f", mime: "image/png", size: 1 }),
  };
  return {
    ...actual,
    useChatSessionStore: Object.assign(
      // The outer literal is fresh per call, but every selector picks a VALUE
      // (winObj reference / primitive) — the memo-relevant identity is stable.
      (selector: (state: unknown) => unknown) =>
        selector({ windows: { [GROUP]: winObj }, sending: { [GROUP]: false } }),
      { getState: () => state },
    ),
  };
});

vi.mock("../../stores/chatStore", () => {
  const state = {
    openSession: vi.fn().mockResolvedValue(undefined),
    seedOptimisticTitle: vi.fn(),
    resolveQuestion: vi.fn(),
  };
  return {
    useChatStore: Object.assign(
      (selector: (state: unknown) => unknown) => selector({ agentStates: {} }),
      { getState: () => state },
    ),
  };
});

const AGENTS_STATE = {
  agents: { [AGENT]: { meta: { display_name: "Agent A", name: "agent-a" } } },
};
vi.mock("../../stores/agentStore", () => ({
  useAgentStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector(AGENTS_STATE),
    { getState: () => ({ selectAgent: vi.fn() }) },
  ),
}));

const GROUP_STATE = {
  activeGroupId: GROUP,
  groups: [
    {
      session_id: GROUP,
      title: "Group",
      participants: [
        { id: ME, identity: "human" },
        { id: AGENT, identity: "agent" },
      ],
    },
  ],
  dms: [],
};
vi.mock("../../stores/projectGroupStore", () => ({
  useProjectGroupStore: (selector: (state: unknown) => unknown) => selector(GROUP_STATE),
}));

vi.mock("../../stores/authStore", () => ({
  useAuthStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector({ account: { user_id: ME } }),
    { getState: () => ({ accessToken: "tok" }) },
  ),
}));

const PROFILE_STATE = {
  profile: { displayName: "You", backendAvatarUrl: null, backendBuiltinAvatarId: null },
};
vi.mock("../../stores/userProfileStore", () => ({
  useUserProfileStore: (selector: (state: unknown) => unknown) => selector(PROFILE_STATE),
}));

vi.mock("../../lib/auth-api", () => ({ fetchDirectory: vi.fn().mockResolvedValue([]) }));
vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://127.0.0.1:1" }));

// One controller object for the whole run: `stickNow` must keep its identity
// (the real hook builds it with useCallback), it is an `onAnswer` dep.
const SCROLL = { ref: { current: null }, stickNow: () => {}, isPinned: () => false, atBottom: true };
vi.mock("../../lib/stickToBottom", () => ({
  useStickToBottom: () => SCROLL,
}));

const TRANSLATION = { t: (key: string, vars?: Record<string, unknown>) => key };
vi.mock("../../i18n/useTranslation", () => ({ useTranslation: () => TRANSLATION }));

vi.mock("../common/AgentAvatar", () => ({ AgentAvatar: () => <div /> }));
vi.mock("../common/UserAvatar", () => ({ UserAvatar: () => <div /> }));
vi.mock("../common/Tooltip", () => ({
  Tooltip: ({ children }: { children?: React.ReactNode }) => <>{children}</>,
}));
vi.mock("./CodeBlock", () => ({ CodeBlock: () => <div /> }));
vi.mock("../editor/markdownLinkResolver", () => ({
  handleChatMarkdownLinkClick: vi.fn(),
  markdownUrlTransform: (url: string) => url,
}));

function agentRow(seq: number, body: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: AGENT,
    kind: "agent_message",
    body,
    to: ME,
    meta: { message_id: `a${seq}` },
  };
}
function userRow(seq: number, body: string): ChatMessageRow {
  return { seq, ts: seq, from: ME, kind: "user_message", body, to: AGENT, meta: { message_id: `u${seq}` } };
}

describe("GroupChatPanel row memo (render-perf L1)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    rowRenders.n = 0;
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
    tail = [userRow(1, "ping")];
    for (let i = 2; i <= 21; i += 1) tail.push(agentRow(i, `body ${i}`));
    winObj = {
      older: [],
      tail,
      tailLoaded: tail.length,
      total: tail.length,
      loadingOlder: false,
    };
  });

  it("a composer keystroke renders ZERO transcript rows", async () => {
    render(<GroupChatPanel />);
    // Flush the one-shot directory fetch: its `setDirectory` gives `nameOf` a
    // new identity exactly once. The baseline must be taken after it settles,
    // or the settle itself (a legitimate prop-identity change) pollutes it.
    await act(async () => {});
    expect(rowRenders.n).toBe(tail.length * 2); // mount + directory settle

    rowRenders.n = 0;
    const ta = screen.getByPlaceholderText("groupChat.placeholderWithMentions") as HTMLTextAreaElement;
    fireEvent.change(ta, { target: { value: "t" } });
    // Host re-rendered (controlled textarea)…
    expect(ta.value).toBe("t");
    // …no row re-rendered.
    expect(rowRenders.n).toBe(0);
  });

  it("a changed row body still repaints (memo must not swallow updates)", async () => {
    const { rerender } = render(<GroupChatPanel />);
    await act(async () => {});
    tail = [tail[0], agentRow(2, "MUTATED answer")];
    winObj = { ...winObj, tail, tailLoaded: tail.length, total: tail.length };
    rerender(<GroupChatPanel />);
    expect(screen.getByText("MUTATED answer")).toBeTruthy();
  });
});
