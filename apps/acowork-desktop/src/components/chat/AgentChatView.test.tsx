import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AgentChatView } from "./AgentChatView";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT = "agent-1";
const SESSION = "chat-1";

// Drill collaborators — the 查看执行 path asserts against these.
let setModeMock: ReturnType<typeof vi.fn>;
let openSessionMock: ReturnType<typeof vi.fn>;
let seedTitleMock: ReturnType<typeof vi.fn>;

// ── chat-session store ────────────────────────────────────────────────
// `windows` / `sending` / `draft` are read through the hook; the composer
// actions through `getState`. The transcript no longer derives any card/turn
// state client-side (ADR-106 D8/D9), so what is left to pin here is routing:
// which row becomes a block, and what the surface hands it.
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
        selector({ windows, sending: sendingMap, draft: drafts, attachedContext: {} }),
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
        seedOptimisticTitle: (...a: unknown[]) => seedTitleMock(...(a as [])),
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

// ADR-106 D3/D8: what a round contains, whether it is still running, and what it
// is parked on are the BLOCK's concern — pinned in ChatExecutionBlock.test.tsx.
// Here it is a stub that records its wiring, so these cases stay about the
// transcript's routing: which row becomes a block, and what the surface hands it.
vi.mock("./ChatExecutionBlock", () => ({
  ChatExecutionBlock: (props: {
    row: ChatMessageRow;
    chatSessionId: string;
    onJumpToExec?: (agentInstanceId: string, execSid: string) => void;
    onReplyToRound?: (rowId: string, quote: string) => void;
  }) => (
    <div
      data-testid="exec-block"
      data-chat={props.chatSessionId}
      data-row={props.row.meta?.message_id}
      data-ref={[
        props.row.meta?.agent_instance_id,
        props.row.meta?.agent_session_id,
        props.row.meta?.round_id,
      ].join("|")}
    >
      <button
        data-testid="block-drill"
        onClick={() =>
          props.onJumpToExec?.(
            props.row.meta?.agent_instance_id ?? "",
            props.row.meta?.agent_session_id ?? "",
          )
        }
      >
        drill
      </button>
      <button
        data-testid="block-reply"
        onClick={() => props.onReplyToRound?.(props.row.meta?.message_id ?? "", "round quote")}
      >
        reply
      </button>
    </div>
  ),
}));

/** The one row shape a round leaves in the chat: a bodyless pointer (D3). */
function roundRow(seq: number, execSid = "exec-9", round = `r${seq}`): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: "agent",
    kind: "execution_round",
    body: "",
    to: "user",
    meta: {
      message_id: `round:${execSid}|${round}`,
      agent_instance_id: AGENT,
      agent_session_id: execSid,
      round_id: round,
      in_reply_to: "u1",
    },
  };
}

function notice(seq: number, meta: Record<string, unknown>): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: "agent",
    kind: "system_notice",
    body: "",
    to: "user",
    meta: { message_id: `n${seq}`, ...meta },
  };
}

function windowOf(...rows: ChatMessageRow[]) {
  return {
    [SESSION]: {
      older: [],
      tail: rows,
      tailLoaded: rows.length,
      total: rows.length,
      loadingOlder: false,
    },
  };
}

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
    seedTitleMock = vi.fn();
    sendTextMock = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal(
      "fetch",
      vi.fn().mockRejectedValue(new Error("no network in test")),
    );
  });

  it("renders an execution_round row as the block that owns it, addressed to this chat (D3/D10)", () => {
    windows = windowOf(userRow(1, "hello there"), roundRow(2));
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    // The human's durable row is still an ordinary bubble.
    expect(screen.getByText("hello there")).toBeTruthy();
    // The pointer becomes exactly one block carrying the chat it must be
    // participation-checked against and the round triple it names — D10 makes
    // the chat part of the address, not a filter.
    const block = screen.getByTestId("exec-block");
    expect(block.getAttribute("data-chat")).toBe(SESSION);
    expect(block.getAttribute("data-ref")).toBe(`${AGENT}|exec-9|r2`);
  });

  it("a block reply addresses the composer to the reference row (D6 level 1)", () => {
    windows = windowOf(userRow(1, "hello there"), roundRow(2));
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    fireEvent.click(screen.getByTestId("block-reply"));
    // The chip names what is being answered; the send itself stays with the
    // composer, so continuing a round is an ordinary turn with an address.
    expect(screen.getByText(/round quote/)).toBeTruthy();
  });

  it("no chat row paints a decision card or a resolution receipt any more (D8/D12)", () => {
    windows = windowOf(
      userRow(1, "hello there"),
      askCardRow(2),
      userRow(3, "B", { in_reply_to: "card2" }),
      notice(4, { notice_type: "interaction_resolved", in_reply_to: "card2", resolution: "answered" }),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    // The card was the round's state, and D8 moved it into the block. A legacy
    // transcript must not resurrect an actionable card, a quote of the question,
    // or the two receipts that used to narrate the answer.
    expect(screen.queryByText("Pick one")).toBeNull();
    expect(screen.queryByText(/chatView\.(cardAnswered|cardUnanswered)/)).toBeNull();
    expect(document.querySelectorAll('[data-testid="exec-block"]')).toHaveLength(0);
    // The answer row is a plain human turn — a bubble, not an event line.
    expect(screen.getByText("B")).toBeTruthy();
  });

  it("retired milestones stay history: no line, no bubble, no invented wait (D9/D12)", () => {
    windows = windowOf(
      userRow(1, "hello there"),
      notice(2, { notice_type: "turn_dispatched", in_reply_to: "u1", agent_session_id: "exec-9" }),
      notice(3, { notice_type: "turn_started", in_reply_to: "u1", agent_session_id: "exec-9" }),
      notice(4, {
        notice_type: "turn_routed",
        in_reply_to: "u1",
        agent_session_id: "exec-9",
        turn_mode: "new",
      }),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    expect(screen.queryAllByText(/chatView\.notice\./)).toHaveLength(0);
    // The routing reaction used to be synthesized into an attributed bubble —
    // a milestone is not speech, so nothing re-attributes and no line appears.
    expect(document.querySelector('[data-event-text="turn"]')).toBeNull();
    // D9: an unanswered human row is not evidence that the agent is busy.
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("the wait line rides this device's in-flight send and nothing else (D9)", () => {
    windows = windowOf(userRow(1, "hello there"), roundRow(2));
    sendingMap = {};
    const { unmount } = render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.queryByRole("status")).toBeNull();
    unmount();

    sendingMap = { [SESSION]: true };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.getByRole("status")).toBeTruthy();
    expect(screen.getByText("chatView.notice.pendingTurnGeneric")).toBeTruthy();
  });

  it("an addressing question asks in centered chrome and offers its answers (D7)", () => {
    windows = windowOf(
      userRow(1, "which one"),
      notice(2, {
        notice_type: "session_choice",
        in_reply_to: "u1",
        options: [
          { session_id: "exec-9", label: "Fix the bug", status: "idle", origin: "existing", why: "same task" },
        ],
        allow_new_task: true,
      }),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    const card = screen.getByTestId("chat-card-options");
    expect(card.textContent).toContain("chatView.block.chooseSession");
    // Transcript chrome, not speech: it is centered like every other notice and
    // never wears the agent's identity, because a question about addressing is
    // not the agent talking to the human.
    expect(card.className).toContain("items-center");
    expect(screen.queryByText("Agent A")).toBeNull();
    // D6's cheap escape rides beside the suggestion, never below a fold.
    expect(screen.getByRole("button", { name: "chatView.block.newTask" })).toBeTruthy();
  });

  it("answering the card re-delivers the human's own words with the chosen address (D6)", async () => {
    windows = windowOf(
      { ...userRow(1, "which one"), attachment_ids: ["f1"] },
      notice(2, {
        notice_type: "session_choice",
        in_reply_to: "u1",
        options: [{ session_id: "exec-9", label: "Fix the bug", status: "idle", origin: "existing" }],
        allow_new_task: true,
      }),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    fireEvent.click(screen.getByRole("button", { name: "Fix the bug" }));
    await waitFor(() => expect(sendTextMock).toHaveBeenCalled());
    // One message, not two: the re-delivery carries the ORIGINAL row id, so the
    // store dedupes it instead of narrating the decision as a new utterance.
    expect(sendTextMock).toHaveBeenCalledWith(SESSION, "which one", undefined, undefined, ["f1"], {
      targetSession: "exec-9",
      messageId: "u1",
    });
  });

  it("an answered card goes quiet rather than offering the same words twice", () => {
    // The round IS the answer (D3): a reference row pointing at the message the
    // card asked about. The buttons stay visible — the record of what was
    // offered must not vanish — but they refuse a second delivery.
    windows = windowOf(
      userRow(1, "which one"),
      notice(2, {
        notice_type: "session_choice",
        in_reply_to: "u1",
        options: [{ session_id: "exec-9", label: "Fix the bug", status: "idle", origin: "existing" }],
        allow_new_task: true,
      }),
      roundRow(3),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect((screen.getByRole("button", { name: "Fix the bug" }) as HTMLButtonElement).disabled).toBe(true);
    expect(sendTextMock).not.toHaveBeenCalled();
  });

  it("the Router's own answer is an attributed bubble (D12)", () => {
    // Only a reply that opened no run is written as `agent_message` now, so it
    // is the agent's speech: avatar, name, bubble.
    windows = windowOf(
      userRow(1, "do the thing"),
      {
        seq: 2,
        ts: 2,
        from: "agent",
        kind: "agent_message",
        body: "here you go",
        to: "user",
        meta: { message_id: "a2", in_reply_to: "u1" },
      },
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    expect(screen.getByText("here you go")).toBeTruthy();
    expect(screen.getByText("Agent A")).toBeTruthy();
    expect(screen.queryAllByTestId("exec-block")).toHaveLength(0);
  });

  it("a bodyless Router answer still states something, not a blank bubble", () => {
    windows = windowOf(
      userRow(1, "do the thing"),
      {
        seq: 2,
        ts: 2,
        from: "agent",
        kind: "agent_message",
        body: "",
        to: "user",
        meta: { message_id: "a2", in_reply_to: "u1" },
      },
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.getByText("chatView.emptyAnswer")).toBeTruthy();
  });

  it("a round is exactly one block — its content, outcome and drill all live there (D3/D8)", () => {
    windows = windowOf(
      userRow(1, "fix this bug"),
      notice(2, { notice_type: "turn_started", in_reply_to: "u1", agent_session_id: "exec-9" }),
      roundRow(3),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    // The answer is not a chat row any more: it is read off the projection and
    // drawn inside the block, so the transcript carries one pointer and one
    // drill, and no milestone line beside them.
    expect(screen.getAllByTestId("exec-block")).toHaveLength(1);
    expect(screen.getAllByTestId("block-drill")).toHaveLength(1);
    expect(screen.queryByText("chatView.notice.turnStarted")).toBeNull();
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("seeds the optimistic exec title off the round's trigger row (ADR-099)", async () => {
    // The reference row is the first durable place the exec session id appears,
    // and its `in_reply_to` names the human message that opened the run.
    windows = windowOf(userRow(1, "fix this bug"), roundRow(2));
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    await waitFor(() =>
      expect(seedTitleMock).toHaveBeenCalledWith(AGENT, "exec-9", "fix this bug"),
    );
  });

  describe("查看执行 drill", () => {
    beforeEach(() => {
      windows = windowOf(userRow(1, "ping"), roundRow(2, "exec-7"));
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
        fireEvent.click(screen.getByTestId("block-drill"));
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
      fireEvent.click(screen.getByTestId("block-drill"));
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
