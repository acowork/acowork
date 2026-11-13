import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AgentChatView } from "./AgentChatView";
import type { ChatDeliveryView, ChatMessageRow } from "../../stores/chatSessionStore";
import type { RunBlock } from "./chatTranscriptGroups";
import { openAttachedRef } from "../../lib/openWorkspaceRef";

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
// The turn this device sent and has not seen an outcome for (ADR-106 D9). The
// real store writes it from the send's response and clears it on the deadline;
// here it is set directly, because the claim's SHAPE is the store's contract and
// what this file pins is what the surface does with it.
let inflightMap: Record<string, unknown> = {};
let settleTurnMock: ReturnType<typeof vi.fn> = vi.fn();
let drafts: Record<string, unknown> = {};
let availableModels: unknown[] = [];
let sendTextMock: ReturnType<typeof vi.fn> = vi.fn();

// The chip's click is the dispatcher under test in the attachment case below;
// every other case here never clicks one.
vi.mock("../../lib/openWorkspaceRef", () => ({ openAttachedRef: vi.fn() }));

vi.mock("../../stores/chatSessionStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../stores/chatSessionStore")>();
  return {
    ...actual,
    useChatSessionStore: Object.assign(
      (selector: (state: unknown) => unknown) =>
        selector({
          windows,
          sending: sendingMap,
          inflight: inflightMap,
          draft: drafts,
          attachedContext: {},
        }),
      {
        getState: () => ({
          sendText: (...a: unknown[]) => sendTextMock(...(a as [])),
          loadOlder: vi.fn().mockResolvedValue(undefined),
          loadNewer: vi.fn().mockResolvedValue(undefined),
          clearLocate: vi.fn(),
          setMode: (...a: unknown[]) => setModeMock(...(a as [])),
          saveDraft: vi.fn().mockResolvedValue(undefined),
          settleTurn: (...a: unknown[]) => settleTurnMock(...(a as [])),
          upload: vi.fn().mockResolvedValue({ id: "a1", filename: "f", mime: "image/png", size: 1 }),
        }),
      },
    ),
  };
});

// The pushed `session_state` per execution session (design 27 §5.3.2: the
// Runtime owns it, the surface only reads it). The wait line's activity word
// comes from here, so a case that wants the agent to look busy sets it.
let chatStates: Record<string, unknown> = {};

vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) =>
      selector({ availableModels, providerAccounts: [], agentStates: chatStates }),
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

// The registry meta the surface reads a responder's face, name and role from.
// `role` is mutable per case: it is what the turn header paints under the name
// (ADR-106 rev. 2026-11-08), and losing it on the way to the header is exactly
// the defect the case below pins.
let agentRole: string | undefined;
vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (state: unknown) => unknown) =>
    selector({
      agents: {
        [AGENT]: { meta: { name: "Agent A", role: agentRole } },
      },
    }),
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
    // The real hook returns `{ t, i18n }`; the turn header reads the locale to
    // join a multi-responder name chain, so the fake has to carry it.
    i18n: { language: "zh-CN" },
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
// The task list reads the agent store's paging state; its own behaviour is
// pinned in ChatAddressPicker.test.tsx. Here it is a marker so the cases below
// stay about one thing: does the gesture open it.
vi.mock("./ChatAddressPicker", () => ({
  ChatAddressPicker: () => <div data-testid="address-picker" />,
}));
vi.mock("../editor/markdownLinkResolver", () => ({
  handleChatMarkdownLinkClick: vi.fn(),
  markdownUrlTransform: (x: string) => x,
}));

// ADR-106 D3/D8: what a round contains, whether it is still running, and what it
// is parked on are the BLOCK's concern — pinned in ChatExecutionBlock.test.tsx.
// Here it is a stub that records its wiring, so these cases stay about the
// transcript's routing: which rows fold into which block, and what the surface
// hands it. The stub echoes the block's own inputs back as attributes, and
// renders the folded question lines so a case can prove the transcript is not
// holding the same sentence twice.
vi.mock("./ChatExecutionBlock", () => ({
  ChatExecutionBlock: (props: {
    block: RunBlock;
    chatSessionId: string;
    showHeader?: boolean;
    onJumpToExec?: (agentInstanceId: string, execSid: string) => void;
    onReplyToRound?: (rowId: string, quote: string) => void;
    onChooseCard?: (card: ChatMessageRow, choice: { sessionId?: string; forceNew?: boolean }) => unknown;
    pendingAskDocked?: boolean;
  }) => (
    <div
      data-testid="exec-block"
      data-chat={props.chatSessionId}
      data-session={`${props.block.instanceId}|${props.block.execSessionId}`}
      data-row={props.block.anchorRow.meta?.message_id}
      data-refs={props.block.rounds.map((r) => r.ref).join(",")}
      data-header={props.showHeader ? "yes" : "no"}
      // The card the layout handed this block as its tail question (D6 level 5).
      data-ask={props.block.pendingAsk?.seq ?? ""}
      // Whether the surface handed the block an answer for the question it
      // hangs at its tail. Echoed, not clicked: the card's own behaviour is
      // pinned in ChatExecutionBlock.test.tsx / ChatNoticeEventRow.test.tsx —
      // what this file owns is the WIRING, and a missing handler here is a card
      // with no button at all.
      data-choose={props.onChooseCard ? "yes" : "no"}
      // Whether the surface docked this run's question at the bottom of the
      // surface (ADR-106 revision 2026-11-11) — then the tail must not draw it
      // a second time.
      data-docked={props.pendingAskDocked ? "yes" : "no"}
    >
      {props.block.rounds.map((r) => (
        <span key={r.ref} data-testid="block-question">
          {r.showUserLine ? (r.triggerRow?.body ?? "") : ""}
        </span>
      ))}
      <button
        data-testid="block-drill"
        onClick={() =>
          props.onJumpToExec?.(props.block.instanceId, props.block.execSessionId)
        }
      >
        drill
      </button>
      <button
        data-testid="block-reply"
        onClick={() =>
          props.onReplyToRound?.(props.block.anchorRow.meta?.message_id ?? "", "round quote")
        }
      >
        reply
      </button>
    </div>
  ),
}));

/** The one row shape a round leaves in the chat: a bodyless pointer (D3). */
function roundRow(
  seq: number,
  execSid = "exec-9",
  round = `r${seq}`,
  inReplyTo = "u1",
): ChatMessageRow {
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
      in_reply_to: inReplyTo,
    },
  };
}

function notice(
  seq: number,
  meta: Record<string, unknown>,
  delivery?: ChatDeliveryView,
): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: "agent",
    kind: "system_notice",
    body: "",
    to: "user",
    meta: { message_id: `n${seq}`, ...meta },
    ...(delivery ? { delivery } : {}),
  };
}

/** What the service projects onto an addressing card (design 31 §6): the
 *  answers it offers and whether a round already answered the message it asked
 *  about. The surface renders this verbatim — the raw `meta.options` are no
 *  longer read by any component. */
function cardDelivery(
  options: Array<{ session_id: string; label?: string; status?: string; origin?: string; why?: string }>,
  answered = false,
  allowNewTask = true,
): ChatDeliveryView {
  return {
    answered: false,
    question: { answered, options, ...(allowNewTask ? { allow_new_task: true } : {}) },
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
  const row: ChatMessageRow = {
    seq,
    ts: seq,
    from: "agent",
    // The retired kind: a pre-ADR-106 transcript held the decision card as its
    // own chat row.
    kind: "interaction_request",
    body: "",
    to: "user",
    meta: { message_id: `card${seq}`, agent_session_id: "exec-9" },
  };
  // The card payload rode `meta.interaction`, which the wire type no longer
  // names — so the legacy row is assembled the way the store used to deliver it.
  (row.meta as Record<string, unknown>).interaction = {
    kind: "ask_question",
    request_id: "q1",
    question: "Pick one",
    options: ["A", "B"],
  };
  return row;
}

/** The composer's send button, disabled or not. No `@testing-library/jest-dom`
 *  in this project, so the `disabled` property is read directly (the idiom
 *  `SplashScreen.candidate.test.tsx` uses too). */
function sendDisabled(): boolean {
  return (
    screen.getByRole("button", {
      name: "chatPanel.sendMessage",
    }) as HTMLButtonElement
  ).disabled;
}

describe("AgentChatView event-log wiring", () => {
  beforeEach(() => {
    chatStates = {};
    vi.clearAllMocks();
    sendingMap = {};
    inflightMap = {};
    agentRole = undefined;
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

    // The pointer becomes exactly one block carrying the chat it must be
    // participation-checked against, the session it belongs to, and the round
    // triple it names — D10 makes the chat part of the address, not a filter.
    const block = screen.getByTestId("exec-block");
    expect(block.getAttribute("data-chat")).toBe(SESSION);
    expect(block.getAttribute("data-session")).toBe(`${AGENT}|exec-9`);
    expect(block.getAttribute("data-refs")).toBe(`${AGENT}|exec-9|r2`);
    // The human's row is the round's question line, so it renders exactly once —
    // inside the block, not as a bubble standing next to it. One sentence twice
    // is the failure mode D6's re-delivery rule already forbids.
    expect(screen.getAllByText("hello there")).toHaveLength(1);
    expect(
      screen.getByText("hello there").closest('[data-testid="exec-block"]'),
    ).toBeTruthy();
  });

  it("the rounds of ONE execution session fold into a single block (ADR-106 D3/D4)", () => {
    // A long-lived run leaves a reference row per round in the chat. Those rows
    // are one session's work, so they are one block: a card per row stacked
    // near-identical boxes down the transcript, which is neither the chat's nor
    // the execution surface's grammar.
    windows = windowOf(
      userRow(1, "first ask"),
      roundRow(2, "exec-9", "r2", "u1"),
      userRow(3, "second ask"),
      roundRow(4, "exec-9", "r4", "u3"),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    const blocks = screen.getAllByTestId("exec-block");
    expect(blocks).toHaveLength(1);
    // Both rounds are in it, oldest first, and the block sits where the session
    // FIRST appeared: a continuation adds a round without moving the block, so
    // the exchange keeps reading as question-then-answer.
    expect(blocks[0]!.getAttribute("data-refs")).toBe(
      `${AGENT}|exec-9|r2,${AGENT}|exec-9|r4`,
    );
    expect(blocks[0]!.getAttribute("data-row")).toBe(`round:exec-9|r2`);
    expect(screen.getAllByTestId("block-question")).toHaveLength(2);
    expect(screen.getAllByText("first ask")).toHaveLength(1);
    expect(screen.getAllByText("second ask")).toHaveLength(1);
  });

  it("two execution sessions in one chat stay two blocks, not one merged run", () => {
    // Folding is per SESSION, not per surface: two execution sessions in one chat
    // are two runs and must not merge into one header.
    windows = windowOf(
      userRow(1, "to the bug"),
      roundRow(2, "exec-9", "r2", "u1"),
      userRow(3, "to the docs"),
      roundRow(4, "exec-10", "r2", "u3"),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    const blocks = screen.getAllByTestId("exec-block");
    expect(blocks).toHaveLength(2);
    expect(blocks.map((b) => b.getAttribute("data-session"))).toEqual([
      `${AGENT}|exec-9`,
      `${AGENT}|exec-10`,
    ]);
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
    // 1:1 names the agent the human is talking to (ADR-100 Rev.2's wording);
    // only the group surface has to stay generic.
    expect(screen.getByText("chatView.notice.pendingTurn|Agent A")).toBeTruthy();
  });

  describe("the wait line says what the agent is doing", () => {
    // Rev. (2026-11-12, the activity word). One sentence for the whole turn is
    // what made a busy agent look slow; the runtime has always pushed the live
    // phase, so the line now reads it. What these cases pin is the honesty
    // rule under the improvement: the word may only come from the run this
    // Chat's own transcript anchors, and never from a session that merely
    // belongs to the same agent.
    const busy = (status: Record<string, unknown>, sid = "exec-9") => ({
      [AGENT]: { sessionStates: { [sid]: { sessionStatus: status } } },
    });

    it("a named dispatch replaces the generic sentence, on the header", () => {
      windows = windowOf(userRow(1, "hello there"), roundRow(2));
      inflightMap = { [SESSION]: { messageId: "u1", at: 1, responders: [AGENT] } };
      chatStates = busy({
        status: "tool_executing",
        detail: { tools: ["memory_recall"] },
      });
      render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
      const header = document.querySelector("[data-turn-header]");
      expect(header).toBeTruthy();
      expect(header!.textContent).toContain("tools.memory_recall.running");
      expect(header!.textContent).not.toContain("chatView.notice.pendingTurnGeneric");
    });

    it("and on the centered line, when no identity resolved", () => {
      windows = windowOf(userRow(1, "hello there"), roundRow(2));
      sendingMap = { [SESSION]: true };
      chatStates = busy({ status: "llm_streaming", detail: { message_id: null } });
      render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
      expect(screen.getByRole("status").textContent).toContain(
        "chatView.activity.composing",
      );
    });

    it("the router-deciding window has no anchor, so it keeps the generic line", () => {
      // The transcript holds no run row yet — the turn may go to a session
      // that does not exist. A word from any other session would be invented.
      windows = windowOf(userRow(1, "hello there"));
      inflightMap = { [SESSION]: { messageId: "u1", at: 1, responders: [AGENT] } };
      chatStates = busy(
        { status: "tool_executing", detail: { tools: ["memory_recall"] } },
        "exec-somewhere-else",
      );
      render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
      const header = document.querySelector("[data-turn-header]");
      expect(header!.textContent).toContain("chatView.notice.pendingTurnGeneric");
      expect(header!.textContent).not.toContain("tools.memory_recall.running");
    });

    it("a session the agent runs for other work is not this turn's activity", () => {
      // The agent's own list can hold a task dispatched from a project group.
      // That run's tool call is a fact about a different conversation, and
      // borrowing its word here would report work this turn never did.
      windows = windowOf(userRow(1, "hello there"), roundRow(2));
      inflightMap = { [SESSION]: { messageId: "u1", at: 1, responders: [AGENT] } };
      chatStates = {
        [AGENT]: {
          sessionStates: {
            "exec-9": { sessionStatus: { status: "idle" } },
            "exec-other": {
              sessionStatus: {
                status: "tool_executing",
                detail: { tools: ["memory_recall"] },
              },
            },
          },
        },
      };
      render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
      const header = document.querySelector("[data-turn-header]");
      expect(header!.textContent).toContain("chatView.notice.pendingTurnGeneric");
      expect(header!.textContent).not.toContain("tools.memory_recall.running");
    });

    it("a dispatch that names nothing falls back without dropping the name", () => {
      // Resume-after-approval cannot say which calls are still in flight. The
      // line must not reuse the previous dispatch's tool, and the 1:1 surface
      // keeps its named sentence.
      windows = windowOf(userRow(1, "hello there"), roundRow(2));
      sendingMap = { [SESSION]: true };
      chatStates = busy({ status: "tool_executing" });
      render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
      expect(screen.getByRole("status").textContent).toContain(
        "chatView.notice.pendingTurn|Agent A",
      );
    });
  });

  it("the claim outliving the POST keeps the line up AND locks the composer (D9)", () => {
    // The window this exists for: the send has returned, the router is still
    // deciding, the Runtime has written nothing. Nothing in the transcript says
    // "busy" — only this device's own send does — and a second send here would
    // race the first turn with no Chat-side queue to absorb it.
    windows = windowOf(userRow(1, "hello there"));
    inflightMap = { [SESSION]: { messageId: "u1", at: 1 } };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.getByRole("status")).toBeTruthy();
    expect(sendDisabled()).toBe(true);
  });

  it("a claim naming its responder wears the turn header (ADR-106 rev 2026-11-08)", () => {
    // The 1:1 peer is a delivery-decision input, not a transcript inference:
    // once the claim carries it, the wait wears the peer's face and name, and
    // the named centered line steps back — one statement, one shape.
    windows = windowOf(userRow(1, "hello there"));
    inflightMap = { [SESSION]: { messageId: "u1", at: 1, responders: [AGENT] } };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    const header = document.querySelector("[data-turn-header]");
    expect(header).toBeTruthy();
    expect(header!.textContent).toContain("Agent A");
    expect(header!.textContent).toContain("chatView.notice.pendingTurnGeneric");
    expect(screen.queryByText("chatView.notice.pendingTurn|Agent A")).toBeNull();
    // The lock is the header's to keep — the upgrade changes the shape, not
    // the claim's teeth.
    expect(sendDisabled()).toBe(true);
  });

  it("the turn header carries the agent's role, as the block header will", () => {
    // Same registry meta, same field: the wait line and the block that takes
    // it over must agree on who is speaking, or the role appears only after
    // the handover and the reader sees the header change, not just settle.
    agentRole = "资深工程师";
    windows = windowOf(userRow(1, "hello there"));
    inflightMap = { [SESSION]: { messageId: "u1", at: 1, responders: [AGENT] } };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    const header = document.querySelector("[data-turn-header]")!;
    expect(header.textContent).toContain("资深工程师");
  });

  it("an unresolvable responder degrades the header to the centered line", () => {
    // A face without a name guesses louder than no face at all: the registry
    // not knowing the id puts the surface back on the old line.
    windows = windowOf(userRow(1, "hello there"));
    inflightMap = { [SESSION]: { messageId: "u1", at: 1, responders: ["ghost-x"] } };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(document.querySelector("[data-turn-header]")).toBeNull();
    expect(screen.getByRole("status")).toBeTruthy();
  });

  it("the round's reference row settles the claim: line gone, composer live", () => {
    // From here the block narrates the run (D3/D9), so a follow-up is allowed —
    // the execution side has a queue for it, the Chat does not need one.
    // The service answered the human's row with a round: that is the fact the
    // wait line ends on, projected onto the row itself (design 31 §6).
    windows = windowOf(
      {
        ...userRow(1, "hello there"),
        delivery: {
          answered: true,
          answered_by: { message_id: "round:exec-9|r2", kind: "execution_round" },
          target: {
            agent_instance_id: AGENT,
            agent_session_id: "exec-9",
            round_id: "r2",
          },
        },
      },
      roundRow(2, "exec-9", "r2", "u1"),
    );
    inflightMap = { [SESSION]: { messageId: "u1", at: 1 } };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.queryByRole("status")).toBeNull();
    expect(sendDisabled()).toBe(false);
    // The surface hands the claim back rather than carrying an id the transcript
    // has already answered.
    expect(settleTurnMock).toHaveBeenCalledWith(SESSION, "u1");
  });

  it("a live delivery alert settles the claim too — it is the outcome the human sees", () => {
    // ADR-105 D5 rev. 2026-11-06: a failed send is no longer a receipt ROW, so
    // nothing in the transcript answers the human's message. The failure is
    // served live on that row (`delivery.alert`) and it IS the answer: the send
    // bounced, and leaving the composer locked on it would read as still in
    // flight.
    windows = windowOf({
      ...userRow(1, "hello there"),
      delivery: {
        answered: false,
        alert: { target: AGENT, error_code: "target_not_installed", expires_in: 600 },
      },
    });
    inflightMap = { [SESSION]: { messageId: "u1", at: 1 } };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.queryByRole("status")).toBeNull();
    expect(sendDisabled()).toBe(false);
  });

  it("a turn whose outcome never lands stops being claimed (deadline)", () => {
    // The store's deadline is what unlocks the composer when a Runtime died
    // between delivery and round open; by then the surface has no claim left,
    // so it must show neither the line nor the lock (pinned store-side in
    // chatSessionStore.test.ts).
    windows = windowOf(userRow(1, "hello there"));
    inflightMap = { [SESSION]: { messageId: "u1", at: 1 } };
    const { rerender } = render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.getByRole("status")).toBeTruthy();
    inflightMap = {};
    rerender(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.queryByRole("status")).toBeNull();
    expect(sendDisabled()).toBe(false);
  });

  it("an addressing question is a card with a header, buttons and one escape (D7)", () => {
    windows = windowOf(
      userRow(1, "which one"),
      notice(
        2,
        { notice_type: "session_choice", in_reply_to: "u1" },
        cardDelivery([
          { session_id: "exec-9", label: "Fix the bug", status: "idle", origin: "existing", why: "same task" },
        ]),
      ),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    const card = screen.getByTestId("chat-card-options");
    // The question is the card's HEADER, and the candidates below it are
    // buttons — not a bordered box of two text lines, which reads as a disabled
    // field and leaves the human unsure what is clickable.
    expect(card.textContent).toContain("chatView.block.chooseSession");
    expect(card.textContent).toContain("chatView.block.unanswered");
    const option = screen.getByRole("button", { name: /Fix the bug/ });
    expect(option.textContent).toContain("same task");
    // The identity above a standalone card is the ROW's author, never the
    // Chat's peer: this row's `from` carries no catalog metadata in this
    // fixture, so it paints the raw id — what it must not do is borrow the
    // Chat agent's face and speak under a name that never asked.
    expect(screen.queryByText("Agent A")).toBeNull();
    // D6's cheap escape rides in the same card, as a footer action rather than a
    // second candidate of equal weight.
    expect(screen.getByRole("button", { name: "chatView.block.newTask" })).toBeTruthy();
  });

  it("answering the card re-delivers the human's own words with the chosen address (D6)", async () => {
    windows = windowOf(
      { ...userRow(1, "which one"), attachment_ids: ["f1"] },
      notice(
        2,
        { notice_type: "session_choice", in_reply_to: "u1" },
        cardDelivery([{ session_id: "exec-9", label: "Fix the bug", status: "idle", origin: "existing" }]),
      ),
    );
    // The words and the files come from the SERVICE now (design 31 §6): the
    // surface no longer rebuilds them from the rows it happens to have loaded,
    // which is what made a card older than the window unanswerable.
    vi.stubGlobal(
      "fetch",
      // Only the card-answer route answers; everything else keeps the file's
      // default "no network in test" so an unrelated background read stays
      // inconclusive rather than parsing a card answer as its own payload.
      vi.fn().mockImplementation((url: string) =>
        String(url).endsWith("/answer")
          ? Promise.resolve({
              status: 200,
              ok: true,
              json: async () => ({
                agent_id: "agent",
                message_id: "u1",
                text: "which one",
                attachment_ids: ["f1"],
                refs: [],
              }),
            })
          : Promise.reject(new Error("no network in test")),
      ),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    fireEvent.click(screen.getByRole("button", { name: "Fix the bug" }));
    await waitFor(() => expect(sendTextMock).toHaveBeenCalled());
    // One message, not two: the re-delivery carries the ORIGINAL row id, so the
    // store dedupes it instead of narrating the decision as a new utterance.
    // The trailing list is the claim's expected responders — in a 1:1 chat
    // that is its peer (ADR-106 revision 2026-11-08, the turn header).
    expect(sendTextMock).toHaveBeenCalledWith(SESSION, "which one", undefined, undefined, ["f1"], {
      targetSession: "exec-9",
      messageId: "u1",
    }, [AGENT]);
  });

  it("hands the run's tail question the same answer handler as a standalone card (D6/D10)", () => {
    // The reported defect: the router asked "continue this task?" on a run, the
    // card was drawn at the block's tail — and had no button at all. The block
    // renders the card read-only unless it is handed `onChooseCard`, and this
    // surface only ever built one for a standalone `row` item, so every
    // block-hosted question was dead chrome. The gate is the CARD's (`to`), not
    // the item's shape: a run item answers through the question hung on it.
    windows = windowOf(
      userRow(1, "继续刚才那个"),
      roundRow(2),
      notice(
        3,
        { notice_type: "confirm_continue" },
        cardDelivery([{ session_id: "exec-9", label: "Fix the bug", status: "idle" }]),
      ),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    const block = screen.getByTestId("exec-block");
    // The card folded into the block it asks about — not left standing as a row
    // (the row path would reach the surface as its own `row` item instead).
    expect(block.getAttribute("data-ask")).toBe("3");
    expect(block.getAttribute("data-choose")).toBe("yes");
  });

  it("docks the run's question at the bottom and tells the run it is docked (ADR-106 rev 2026-11-11)", () => {
    // The run is anchored where its exchange STARTED, so a question drawn at
    // its tail could sit hundreds of rows above the composer, in a transcript
    // that does not scroll on its own: the human typed at the bottom and saw
    // nothing happen. The dock parks the question over the composer's edge,
    // and the run is told so — its tail must stay empty, one question in one
    // place.
    windows = windowOf(
      userRow(1, "继续刚才那个"),
      roundRow(2),
      notice(
        3,
        { notice_type: "confirm_continue" },
        cardDelivery([{ session_id: "exec-9", label: "Fix the bug", status: "idle" }]),
      ),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    const dock = screen.getByTestId("chat-ask-dock");
    expect(dock.textContent).toContain("chatView.block.confirmSession");
    // The card in the dock is the shared one, with its own answer path.
    expect(dock.querySelector('[data-testid="chat-card-options"]')).not.toBeNull();
    expect(screen.getByRole("button", { name: /Fix the bug/ })).toBeTruthy();
    // The dock is not a transcript line: it names the agent asking, but wears
    // no 40px gutter of its own.
    expect(dock.querySelector('[data-testid="chat-card-author"]')).not.toBeNull();
    expect(dock.querySelector(".w-10")).toBeNull();
    // The run is the question's subject and knows the question is docked.
    expect(screen.getByTestId("exec-block").getAttribute("data-docked")).toBe("yes");
  });

  it("a standalone question card stays a transcript line — the dock is for the run's own question", () => {
    // The multi-candidate card (D6 level 3) has no one run to belong to, so it
    // keeps standing where it arrived, with the agent's face over it. Only the
    // run's own question (level 5) is docked.
    windows = windowOf(
      userRow(1, "which one"),
      notice(
        2,
        { notice_type: "session_choice", in_reply_to: "u1" },
        cardDelivery([{ session_id: "exec-9", label: "Fix the bug", status: "idle" }]),
      ),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.queryByTestId("chat-ask-dock")).toBeNull();
    expect(screen.getByTestId("chat-card-options")).toBeTruthy();
  });

  it("an answered card leaves the transcript to the round's block", () => {
    // The round IS the answer (D3), and the SERVICE says so through
    // `question.answered`. The card then paints nothing: it was a question, and
    // the block below already names the session, its rounds and a way in. A
    // disabled row of buttons kept the offer on screen; a receipt line repeated
    // the block. Neither is a record of the conversation.
    windows = windowOf(
      userRow(1, "which one"),
      notice(
        2,
        { notice_type: "session_choice", in_reply_to: "u1" },
        {
          answered: false,
          question: {
            answered: true,
            options: [
              { session_id: "exec-9", label: "Fix the bug", status: "idle", origin: "existing" },
            ],
            allow_new_task: true,
          },
        },
      ),
      roundRow(3),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(screen.queryByTestId("chat-card-options")).toBeNull();
    expect(screen.queryByTestId("chat-card-answered")).toBeNull();
    expect(screen.queryByRole("button", { name: /Fix the bug/ })).toBeNull();
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

// The `@` gesture on the 1:1 composer (ADR-106 D7). It used to advertise
// "Enter to send, Shift+Enter for a new line" — mechanics every IM user already
// knows — while the one thing this surface actually offers (name the task the
// message continues) was reachable only through an unlabeled toolbar glyph.
describe("AgentChatView `@` composer gesture", () => {
  beforeEach(() => {
    windows = {};
    drafts = {};
    availableModels = [];
    setModeMock = vi.fn();
    openSessionMock = vi.fn().mockResolvedValue(undefined);
    sendTextMock = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  const composer = () =>
    screen.getByPlaceholderText("chatView.address.composerHint") as HTMLTextAreaElement;

  it("advertises the `@` gesture where the Enter hint used to be", () => {
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    expect(composer()).toBeTruthy();
  });

  it("typing `@` opens the task list and writes nothing into the draft", () => {
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    const ta = composer();
    expect(screen.queryByTestId("address-picker")).toBeNull();
    // `fireEvent` returns false once a handler cancels the default action —
    // here that cancellation IS the contract: the `@` must not land in the body,
    // because the address rides the chip above the composer instead.
    expect(fireEvent.keyDown(ta, { key: "@" })).toBe(false);
    expect(screen.getByTestId("address-picker")).toBeTruthy();
    expect(ta.value).toBe("");
  });

  it("leaves a mid-word `@` alone, so an email address stays an email", () => {
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    const ta = composer();
    fireEvent.change(ta, { target: { value: "mail me at nick" } });
    ta.setSelectionRange(ta.value.length, ta.value.length);
    expect(fireEvent.keyDown(ta, { key: "@" })).toBe(true);
    expect(screen.queryByTestId("address-picker")).toBeNull();

    // Same key after a space is a word opening, so it is the gesture again.
    fireEvent.change(ta, { target: { value: "mail me at " } });
    ta.setSelectionRange(ta.value.length, ta.value.length);
    expect(fireEvent.keyDown(ta, { key: "@" })).toBe(false);
    expect(screen.getByTestId("address-picker")).toBeTruthy();
  });

  it("does not hijack `@` while an IME is still composing it", () => {
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    const ta = composer();
    expect(
      fireEvent.keyDown(ta, { key: "@", isComposing: true }),
    ).toBe(true);
    expect(screen.queryByTestId("address-picker")).toBeNull();
  });

  it("the toolbar `@` still toggles the same list", () => {
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    const btn = screen.getByLabelText("chatView.address.pick");
    fireEvent.click(btn);
    expect(screen.getByTestId("address-picker")).toBeTruthy();
    fireEvent.click(btn);
    expect(screen.queryByTestId("address-picker")).toBeNull();
  });

  it("anchors the list to whichever gesture opened it", () => {
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    const btn = screen.getByLabelText("chatView.address.pick");
    // Clicked: the popup hangs off the toolbar button it came from.
    fireEvent.click(btn);
    expect(
      screen
        .getByTestId("address-picker")
        .closest('[data-toolbar-cluster="right"]'),
    ).not.toBeNull();
    fireEvent.click(btn);
    // Typed: it belongs above the composer, not under the toolbar.
    fireEvent.keyDown(screen.getByRole("textbox"), {
      key: "@",
      target: { selectionStart: 0 },
    });
    const typed = screen.getByTestId("address-picker");
    expect(typed.closest('[data-toolbar-cluster="right"]')).toBeNull();
  });

  it("the workspace-ref banner a turn carried opens its file (D12 Rev. 2026-11)", () => {
    // The reported defect: in the execution session the banner hanging off a
    // message opened the file, in the Chat session it only painted. Same chip
    // frame, same glyph, same dispatcher — the difference was that the label
    // had no path to resolve. It has one now.
    windows = windowOf(
      userRow(1, "look at this", {
        exec_attachments: [
          { filename: "src/main.rs", kind: "attached_file", absPath: "E:/ws/src/main.rs" },
        ],
      }),
    );
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);
    fireEvent.click(screen.getByRole("button", { name: "src/main.rs" }));
    expect(openAttachedRef).toHaveBeenCalledWith({
      item: { type: "attached_file", absPath: "E:/ws/src/main.rs", name: "src/main.rs" },
      agentId: AGENT,
      currentWorkspaceId: "",
    });
  });
});
