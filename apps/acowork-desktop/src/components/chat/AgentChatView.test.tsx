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
// actions through `getState`. The pure read projections (`closedCardIds`,
// `humanAnsweredCardIds`, …) are kept REAL via importOriginal so the view's
// event-row + inline-card wiring is exercised against the actual rules.
let windows: Record<string, unknown>;
// `sending` is the in-flight HTTP window; the view reads one flag per session.
let sendingMap: Record<string, unknown> = {};
let drafts: Record<string, unknown> = {};
let availableModels: unknown[] = [];
let sendTextMock: ReturnType<typeof vi.fn> = vi.fn();

vi.mock("../../stores/chatSessionStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../stores/chatSessionStore")>();
  return {
    ...actual,
    useChatSessionStore: Object.assign(
      (selector: (state: unknown) => unknown) =>
        selector({ windows, sending: sendingMap, draft: drafts }),
      {
        getState: () => ({
          sendText: (...a: unknown[]) => sendTextMock(...(a as [])),
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
    {
      getState: () => ({
        openSession: (...a: unknown[]) => openSessionMock(...(a as [])),
        // Optimistic-title seed (ADR-099): the event-row effect calls this once
        // per resolved exec session; stubbed here — the store's own suite
        // (chatStore.sessionTitle.test.ts) covers its real behaviour.
        seedOptimisticTitle: vi.fn(),
        resolveQuestion: vi.fn(),
      }),
    },
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
  // Echo keys (with interpolated values appended) so assertions read against
  // the i18n vocabulary directly.
  useTranslation: () => ({
    t: (key: string, vars?: Record<string, unknown>) =>
      vars && Object.keys(vars).length ? `${key}|${Object.values(vars).join("|")}` : key,
  }),
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

describe("AgentChatView event-log wiring", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    sendingMap = {};
    setModeMock = vi.fn();
    openSessionMock = vi.fn().mockResolvedValue(undefined);
    sendTextMock = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal(
      "fetch",
      vi.fn().mockRejectedValue(new Error("no network in test")),
    );
  });

  it("renders an unanswered ask_question as an inline card that answers in place", () => {
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

    // The interaction_request row is a durable, actionable card: its question
    // and option list render in the stream.
    expect(screen.getByText("Pick one")).toBeTruthy();

    // Select an option radio then submit to answer through the chat channel,
    // bound to the card id (the answer channel's in_reply_to target).
    fireEvent.click(screen.getByText("A"));
    fireEvent.click(screen.getByText("askQuestionCard.submit"));
    expect(sendTextMock).toHaveBeenCalledWith(SESSION, "A", "card2");
  });

  it("renders a chat-surface answer as an event line, not a bubble", () => {
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

    // No live card once the human answered.
    expect(screen.queryByText("chatView.freeAnswer")).toBeNull();
    // No receipt either — it repeated the question without saying who answered
    // or what they said. The slot is empty now. (The negative lookahead keeps
    // this about the removed `cardAnswered` receipt key, not the new
    // `cardAnsweredNote` / `cardAnsweredInExec` event lines.)
    expect(screen.queryByText(/chatView\.cardAnswered(?!Note|InExec)/)).toBeNull();
    // And the slot must not fall through to the unanswered note either: an
    // answered card that reads "Unanswered: …" is worse than the receipt was.
    expect(screen.queryByText(/chatView.cardUnanswered/)).toBeNull();
    // The answer is a step of the run, not an utterance: a row-tier event
    // line naming the choice — no bubble, no avatar, no quote of the question
    // (the question was just read in the open card above).
    expect(screen.getByText("chatView.cardAnsweredNote|B")).toBeTruthy();
    expect(screen.queryByText("Pick one")).toBeNull();
    expect(document.querySelector('[data-event-text="turn"]')).toBeTruthy();
  });

  it("renders an exec-surface answer as one event line and dedups the notice", () => {
    const resolvedNotice = (extra: Record<string, unknown>): ChatMessageRow => ({
      seq: 9,
      ts: 9,
      from: "agent",
      kind: "system_notice",
      body: "",
      to: "user",
      meta: {
        message_id: "ir:card2",
        notice_type: "interaction_resolved",
        in_reply_to: "card2",
        ...extra,
      },
    });
    windows = {
      [SESSION]: {
        older: [],
        tail: [
          userRow(1, "hello there"),
          askCardRow(2),
          resolvedNotice({ resolution: "answered" }),
        ],
        tailLoaded: 3,
        total: 3,
        loadingOlder: false,
      },
    };
    const { unmount } = render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    // The decision happened on the execution surface — the chat says so, once,
    // as an event line; the card paints nothing and never claims "unanswered".
    expect(screen.getAllByText("chatView.cardAnsweredInExec")).toHaveLength(1);
    expect(screen.queryByText(/chatView.cardUnanswered/)).toBeNull();
    unmount();

    // Answered on the chat surface instead: the answer row states the fact,
    // and the Runtime's duplicate notice stays silent.
    windows = {
      [SESSION]: {
        older: [],
        tail: [
          userRow(1, "hello there"),
          askCardRow(2),
          userRow(3, "B", { in_reply_to: "card2" }),
          resolvedNotice({ resolution: "answered" }),
        ],
        tailLoaded: 4,
        total: 4,
        loadingOlder: false,
      },
    };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.getByText("chatView.cardAnsweredNote|B")).toBeTruthy();
    expect(screen.queryByText("chatView.cardAnsweredInExec")).toBeNull();
  });

  it("drops a retired milestone from an old transcript and derives the wait line", () => {
    windows = {
      [SESSION]: {
        older: [],
        tail: [
          userRow(1, "hello there"),
          {
            seq: 2,
            ts: 2,
            from: "agent",
            kind: "system_notice",
            body: "",
            to: "user",
            meta: {
              message_id: "td:u1",
              notice_type: "turn_dispatched",
              in_reply_to: "u1",
              agent_session_id: "exec-9",
            },
          },
        ],
        tailLoaded: 2,
        total: 2,
        loadingOlder: false,
      },
    };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    // The milestone is runtime state now (ADR-100 revision): a transcript that
    // still carries the old row must not paint it back.
    expect(screen.queryByText("chatView.notice.turnDispatched")).toBeNull();
    // The reader gets one derived line instead: the newest human message has
    // no answer yet, so the agent is still working on it.
    expect(screen.getByText("chatView.notice.pendingTurn|Agent A")).toBeTruthy();
  });

  it("renders the routing reaction as an attributed bubble and keeps the wait", () => {
    // ADR-100 D11: "启动新任务：xxx" is the agent's own conversational answer
    // to the user message — avatar + name + bubble, never a centered line —
    // while the live line below it keeps covering the run in flight.
    windows = {
      [SESSION]: {
        older: [],
        tail: [
          userRow(1, "review the repo"),
          {
            seq: 2,
            ts: 2,
            from: "agent",
            kind: "system_notice",
            body: "",
            to: "user",
            meta: {
              message_id: "tr:u1",
              notice_type: "turn_routed",
              in_reply_to: "u1",
              agent_session_id: "exec-9",
              turn_mode: "new",
            },
          },
        ],
        tailLoaded: 2,
        total: 2,
        loadingOlder: false,
      },
    };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    // The bubble names the task off the trigger body (no title exists yet).
    expect(screen.getByText("chatView.notice.turnRoutedNew|review the repo")).toBeTruthy();
    // Attributed: the agent's name header rides with it.
    expect(screen.getByText("Agent A")).toBeTruthy();
    // The run is still in flight — the derived wait line stays.
    expect(screen.getByText("chatView.notice.pendingTurn|Agent A")).toBeTruthy();
  });

  it("keeps the avatar header on the reply after an empty terminal row", () => {
    // The header predicate folded the avatar off any `agent_message` whose
    // predecessor was ALSO an `agent_message` — but an empty terminal row
    // paints as a centered status line, not a bubble, so the attributed reply
    // under it lost its avatar and stood "孤零零" in the stream.
    windows = {
      [SESSION]: {
        older: [],
        tail: [
          userRow(1, "do the thing"),
          {
            seq: 2,
            ts: 2,
            from: "agent",
            kind: "agent_message",
            body: "",
            to: "user",
            meta: { message_id: "a2", in_reply_to: "u1", status: "aborted", sender_session_id: "exec-8" },
          },
          {
            seq: 3,
            ts: 3,
            from: "agent",
            kind: "agent_message",
            body: "late answer",
            to: "user",
            meta: { message_id: "a3", in_reply_to: "u1", status: "completed", sender_session_id: "exec-9" },
          },
        ],
        tailLoaded: 3,
        total: 3,
        loadingOlder: false,
      },
    };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    expect(screen.getByText("late answer")).toBeTruthy();
    // The centered line owns no header, so the reply re-attributes.
    expect(screen.getByText("Agent A")).toBeTruthy();
  });

  it("ends the wait line at the terminal reply — one bubble, one drill", () => {
    windows = {
      [SESSION]: {
        older: [],
        tail: [
          userRow(1, "fix this bug"),
          {
            seq: 2,
            ts: 2,
            from: "agent",
            kind: "system_notice",
            body: "",
            to: "user",
            meta: {
              message_id: "ts:u1",
              notice_type: "turn_started",
              in_reply_to: "u1",
              agent_session_id: "exec-9",
            },
          },
          {
            seq: 3,
            ts: 3,
            from: "agent",
            kind: "agent_message",
            body: "on it, done",
            to: "user",
            meta: {
              message_id: "a3",
              in_reply_to: "u1",
              status: "completed",
              agent_session_id: "exec-9",
            },
          },
        ],
        tailLoaded: 3,
        total: 3,
        loadingOlder: false,
      },
    };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    expect(screen.getByText("on it, done")).toBeTruthy();
    // Neither milestone reached the stream, and the wait line is gone now that
    // a terminal row answers the turn.
    expect(screen.queryByText("chatView.notice.turnStarted")).toBeNull();
    expect(screen.queryByText(/chatView\.notice\.pendingTurn/)).toBeNull();
    // One run to drill into, so the 查看执行 affordance appears exactly once.
    expect(screen.getAllByText("chatView.viewRun")).toHaveLength(1);
  });

  it("paints an empty terminal row as a centered outcome, not an agent bubble", () => {
    windows = {
      [SESSION]: {
        older: [],
        tail: [
          userRow(1, "fix this bug"),
          {
            seq: 2,
            ts: 2,
            from: "agent",
            kind: "agent_message",
            body: "",
            to: "user",
            meta: {
              message_id: "a2",
              in_reply_to: "u1",
              status: "aborted",
              sender_session_id: "exec-9",
            },
          },
        ],
        tailLoaded: 2,
        total: 2,
        loadingOlder: false,
      },
    };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    // The outcome IS the message: centered, with the drill into the run.
    expect(screen.getByText("chatView.notice.interrupted")).toBeTruthy();
    expect(screen.getByText("chatView.viewRun")).toBeTruthy();
    // The empty-bubble placeholder must not stand in for a real reply.
    expect(screen.queryByText("chatView.emptyAnswer")).toBeNull();
    // A terminal row answers the turn, so the wait line is gone.
    expect(screen.queryByText(/chatView\.notice\.pendingTurn/)).toBeNull();
  });

  it("shows the generic wait line while the send round trip is in flight", () => {
    windows = {
      [SESSION]: { older: [], tail: [], tailLoaded: 0, total: 0, loadingOlder: false },
    };
    sendingMap = { [SESSION]: true };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    // The transcript has no human row yet — the line is still owed to the user.
    expect(screen.getByRole("status")).toBeTruthy();
    expect(screen.getByText("chatView.notice.pendingTurn|Agent A")).toBeTruthy();
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
      await waitFor(() => expect(openSessionMock).toHaveBeenCalledWith(AGENT, "exec-7", { prepend: true }));
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
    sendTextMock = vi.fn().mockResolvedValue(undefined);
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
