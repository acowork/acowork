import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AgentChatView } from "./AgentChatView";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT = "agent-1";
const SESSION = "chat-1";

// Drill collaborators — the 查看执行 path asserts against these.
let setModeMock: ReturnType<typeof vi.fn>;
let openSessionMock: ReturnType<typeof vi.fn>;

// ── chat-session store ────────────────────────────────────────────────
// `windows` / `sending` / `draft` are read through the hook; the composer
// actions through `getState`. The pure read projections (`projectLiveTurn`,
// `closedCardIds`, …) are kept REAL via importOriginal so the view's card +
// processing-dot wiring is exercised against the actual projection rules.
let windows: Record<string, unknown>;
let drafts: Record<string, unknown> = {};
let availableModels: unknown[] = [];

vi.mock("../../stores/chatSessionStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../stores/chatSessionStore")>();
  return {
    ...actual,
    useChatSessionStore: Object.assign(
      (selector: (state: unknown) => unknown) =>
        selector({ windows, sending: {}, draft: drafts }),
      {
        getState: () => ({
          sendText: vi.fn().mockResolvedValue(undefined),
          loadOlder: vi.fn().mockResolvedValue(undefined),
          loadNewer: vi.fn().mockResolvedValue(undefined),
          clearLocate: vi.fn(),
          setMode: (...a: unknown[]) => setModeMock(...(a as [])),
          saveDraft: vi.fn().mockResolvedValue(undefined),
          upload: vi.fn().mockResolvedValue({ id: "a1", filename: "f", mime: "image/png", size: 1 }),
        }),
      },
    ),
  };
});

vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) =>
      selector({ availableModels, providerAccounts: [] }),
    { getState: () => ({ openSession: (...a: unknown[]) => openSessionMock(...(a as [])) }) },
  ),
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (state: unknown) => unknown) =>
    selector({ agents: { [AGENT]: { meta: { display_name: "Agent A", name: "agent-a" } } } }),
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

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

// The unified status card is the collaborator under test: stub it so its
// presence/absence and the props the view wires into it are observable,
// without pulling the whole live-mirror store graph.
vi.mock("./ExecSessionStatusCard", () => ({
  ExecSessionStatusCard: ({ card }: { card?: { kind: string; messageId: string } }) => (
    <div
      data-testid="status-card"
      data-kind={card?.kind ?? ""}
      data-msgid={card?.messageId ?? ""}
    />
  ),
}));

// Toolbar / editor deps that pull unrelated stores — neutral stubs.
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
  markdownUrlTransform: (x: string) => x,
}));

function userRow(seq: number, body: string, extra?: Record<string, unknown>): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: "user",
    kind: "user_message",
    body,
    to: "agent",
    meta: { message_id: `u${seq}`, ...(extra ?? {}) },
  };
}

function askCardRow(seq: number): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: "agent",
    kind: "interaction_request",
    body: "",
    to: "user",
    meta: {
      message_id: `card${seq}`,
      agent_session_id: "exec-9",
      interaction: {
        kind: "ask_question",
        request_id: "q1",
        question: "Pick one",
        options: ["A", "B"],
      },
    },
  };
}

describe("AgentChatView unified-card wiring", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    setModeMock = vi.fn();
    openSessionMock = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal(
      "fetch",
      vi.fn().mockRejectedValue(new Error("no network in test")),
    );
  });

  it("renders an unanswered ask_question ONLY through the unified card, not a standalone row", () => {
    windows = {
      [SESSION]: {
        older: [],
        tail: [userRow(1, "hello there"), askCardRow(2)],
        tailLoaded: 2,
        total: 2,
        loadingOlder: false,
      },
    };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    // The durable user_message paints as a normal bubble.
    expect(screen.getByText("hello there")).toBeTruthy();

    // The interaction_request row is suppressed: its question never renders
    // as a standalone line anywhere in the stream.
    expect(screen.queryByText("Pick one")).toBeNull();

    // …and is instead carried by the unified status card, bound to the card's
    // kind + message id (the answer channel's in_reply_to target).
    const card = screen.getByTestId("status-card");
    expect(card.getAttribute("data-kind")).toBe("ask_question");
    expect(card.getAttribute("data-msgid")).toBe("card2");
  });

  it("drops the card and leaves no orphan row once the card is answered", () => {
    windows = {
      [SESSION]: {
        older: [],
        tail: [
          userRow(1, "hello there"),
          askCardRow(2),
          userRow(3, "B", { in_reply_to: "card2" }),
        ],
        tailLoaded: 3,
        total: 3,
        loadingOlder: false,
      },
    };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    // No live card once every projection is answered (inFlight absent too).
    expect(screen.queryByTestId("status-card")).toBeNull();
    // No "已回答" / question orphan — the answered card row paints nothing.
    expect(screen.queryByText("Pick one")).toBeNull();
    // The answer itself is just a normal user bubble.
    expect(screen.getByText("B")).toBeTruthy();
  });

  describe("查看执行 drill", () => {
    function agentRow(seq: number, body: string, extra?: Record<string, unknown>): ChatMessageRow {
      return {
        seq,
        ts: seq,
        from: "agent",
        kind: "agent_message",
        body,
        to: "user",
        meta: { message_id: `a${seq}`, ...(extra ?? {}) },
      };
    }

    beforeEach(() => {
      windows = {
        [SESSION]: {
          older: [],
          tail: [userRow(1, "ping"), agentRow(2, "pong", { sender_session_id: "exec-7" })],
          tailLoaded: 2,
          total: 2,
          loadingOlder: false,
        },
      };
    });

    it("toasts instead of jumping when the existence probe answers 404", async () => {
      // A deleted exec session must not cold-open an empty exec tab with the
      // load-error + retry banner: the drill probes `GET /sessions/{sid}`
      // first, and a definitive 404 is answered with a toast and no navigation.
      vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ status: 404 }));
      const toasts: Event[] = [];
      const listener = (e: Event) => toasts.push(e);
      window.addEventListener("acowork:toast", listener);
      try {
        render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
        fireEvent.click(screen.getByText("chatView.viewRun"));
        await waitFor(() => expect(toasts.length).toBe(1));
        expect(openSessionMock).not.toHaveBeenCalled();
        expect(setModeMock).not.toHaveBeenCalled();
        const detail = (toasts[0] as CustomEvent<{ message: string }>).detail;
        expect(detail.message).toBe("chatView.execSessionGone");
      } finally {
        window.removeEventListener("acowork:toast", listener);
      }
    });

    it("jumps when the probe is inconclusive (agent stopped / no network)", async () => {
      // Non-404 outcomes (a rejected fetch here) must fall through to the
      // existing best-effort open — a probe hiccup never blocks a live session.
      vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
      render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
      fireEvent.click(screen.getByText("chatView.viewRun"));
      await waitFor(() => expect(openSessionMock).toHaveBeenCalledWith(AGENT, "exec-7"));
      await waitFor(() => expect(setModeMock).toHaveBeenCalledWith(AGENT, "exec"));
    });
  });
});

// Regression: the composer used to resolve the effort label inline as
// `draft.reasoning_effort ?? default_reasoning_effort ?? null`, skipping the
// "reasoning-capable -> auto" level that resolveDefaultReasoningEffort (and
// the backend) apply. A custom-provider model with capabilities but no
// configured default therefore showed no toggle on the Chat surface while
// the execution surface showed one.
describe("AgentChatView reasoning-effort toggle", () => {
  beforeEach(() => {
    windows = {};
    drafts = { [SESSION]: { model: "qwen3.8-flash", provider: "custom-loostone" } };
    availableModels = [];
    setModeMock = vi.fn();
    openSessionMock = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  it("shows the toggle for a reasoning-capable model with no default effort", () => {
    availableModels = [{ name: "qwen3.8-flash", provider: "custom-loostone", reasoning: true }];
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.getByTestId("effort-menu")).toBeTruthy();
  });

  it("shows the toggle when the model has a configured default effort", () => {
    availableModels = [
      { name: "qwen3.8-flash", provider: "custom-loostone", reasoning: true, default_reasoning_effort: "medium" },
    ];
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.getByTestId("effort-menu")).toBeTruthy();
  });

  it("hides the toggle for a model that does not support reasoning", () => {
    availableModels = [{ name: "qwen3.8-flash", provider: "custom-loostone", reasoning: false }];
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.queryByTestId("effort-menu")).toBeNull();
  });
});
