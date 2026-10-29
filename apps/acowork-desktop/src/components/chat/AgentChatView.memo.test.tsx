// @vitest-environment jsdom
/**
 * Row-level memo contract of the 1:1 chat surface (chat-transcript
 * render-perf plan L1).
 *
 * The defect this locks: a composer keystroke re-renders the host, the host
 * re-runs `rows.map`, and until L1 nothing below the host was memoized — so
 * every loaded markdown row re-parsed per keystroke (~2s on a long session).
 *
 * The pin: with `RowView` memoized and every host→row prop reference-stable,
 * a keystroke must produce ZERO row renders. Row renders are counted through
 * `useSessionTitle` — the one hook `RowView` calls unconditionally on every
 * render and nowhere else in this tree.
 *
 * The mocks mirror the real app's stability contract: zustand actions are
 * created once (so `getState()` hands the SAME object every call) and
 * `useTranslation` hands the SAME `t` between language changes. A harness
 * that rebuilt these per call would itself defeat the memo and test nothing.
 */
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ChangeEvent } from "react";
import { AgentChatView } from "./AgentChatView";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT = "agent-1";
const SESSION = "chat-1";

/** Counts `RowView` renders (one per render — see header). */
const rowRenders = vi.hoisted(() => ({ n: 0 }));

let windows: Record<string, unknown>;
let tail: ChatMessageRow[] = [];

function makeWindow() {
  return {
    [SESSION]: {
      older: [],
      tail,
      tailLoaded: tail.length,
      total: tail.length,
      loadingOlder: false,
    },
  };
}

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
    sendText: vi.fn().mockResolvedValue(undefined),
    loadOlder: vi.fn().mockResolvedValue(undefined),
    loadNewer: vi.fn().mockResolvedValue(undefined),
    clearLocate: vi.fn(),
    setMode: vi.fn(),
    saveDraft: vi.fn().mockResolvedValue(undefined),
    upload: vi.fn().mockResolvedValue({ id: "a1", filename: "f", mime: "image/png", size: 1 }),
  };
  return {
    ...actual,
    useChatSessionStore: Object.assign(
      (selector: (state: unknown) => unknown) =>
        selector({ windows, sending: {}, draft: {}, attachedContext: {} }),
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
      (selector: (state: unknown) => unknown) =>
        selector({ availableModels: [], providerAccounts: [], agentStates: {} }),
      { getState: () => state },
    ),
  };
});

const AGENTS_STATE = { agents: { [AGENT]: { meta: { display_name: "Agent A", name: "agent-a" } } } };
vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (state: unknown) => unknown) => selector(AGENTS_STATE),
}));

vi.mock("../../stores/authStore", () => ({
  useAuthStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector({ accessToken: "tok" }),
    { getState: () => ({ accessToken: "tok" }) },
  ),
}));

vi.mock("../../stores/userProfileStore", () => ({
  useUserProfileStore: (selector: (state: unknown) => unknown) =>
    selector({
      profile: { displayName: "You", backendAvatarUrl: null, backendBuiltinAvatarId: null },
    }),
}));

vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://127.0.0.1:1" }));

const TRANSLATION = { t: (key: string, vars?: Record<string, unknown>) => key };
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => TRANSLATION,
}));

vi.mock("./ChatPanel", () => ({
  ModelMenu: () => <div data-testid="model-menu" />,
  ReasoningEffortMenu: () => <div data-testid="effort-menu" />,
}));
vi.mock("../workspace/WorkspaceSelector", () => ({
  WorkspaceSelector: () => <div data-testid="workspace-selector" />,
}));
vi.mock("./CodeBlock", () => ({ CodeBlock: () => <div /> }));
vi.mock("../common/UserAvatar", () => ({ UserAvatar: () => <div /> }));
vi.mock("../common/AgentAvatar", () => ({ AgentAvatar: () => <div /> }));
vi.mock("../common/Tooltip", () => ({
  Tooltip: ({ children }: { children?: React.ReactNode }) => <>{children}</>,
}));
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
    to: "user",
    meta: { message_id: `a${seq}` },
  };
}
function userRow(seq: number, body: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: "user",
    kind: "user_message",
    body,
    to: AGENT,
    meta: { message_id: `u${seq}` },
  };
}

describe("AgentChatView row memo (render-perf L1)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    rowRenders.n = 0;
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
    tail = [];
  });

  it("a composer keystroke renders ZERO transcript rows", () => {
    tail = [userRow(1, "ping")];
    for (let i = 2; i <= 21; i += 1) tail.push(agentRow(i, `body ${i}`));
    windows = makeWindow();

    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    // Every row mounted exactly once (the hook runs once per RowView render).
    expect(rowRenders.n).toBe(tail.length);

    rowRenders.n = 0;
    const ta = screen.getByPlaceholderText("chatPanel.inputMessage") as HTMLTextAreaElement;
    fireEvent.change(ta, { target: { value: "t" } });
    // The host re-rendered (the controlled textarea proves it)…
    expect(ta.value).toBe("t");
    // …and no row re-rendered: every host→row prop kept its reference.
    expect(rowRenders.n).toBe(0);
  });

  it("a changed row body still repaints (memo must not swallow updates)", () => {
    tail = [userRow(1, "ping"), agentRow(2, "old answer")];
    windows = makeWindow();
    const { rerender } = render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    // Simulate a window refresh: new row objects (as a refetch produces), with
    // the last row's body changed.
    tail = [tail[0], agentRow(2, "MUTATED answer")];
    windows = makeWindow();
    rerender(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    expect(screen.getByText("MUTATED answer")).toBeTruthy();
  });
});
