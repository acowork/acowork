// @vitest-environment jsdom
/**
 * The group surface waits on the same claim the 1:1 surface does (ADR-106 D9):
 * a send THIS device made, keyed by the id its own response carried, ending at
 * the first row that answers that id.
 *
 * The case that matters is the one the old `sending` flag could not cover: the
 * POST has already returned, the router is still deciding, and the transcript
 * holds nothing. Before the claim was restored, that window showed no line and
 * left the send button live — a second message racing a turn still being
 * routed, with no Chat-side queue to absorb it. The group stays generic in
 * wording because one utterance fans out to several agents, and the fan-out's
 * archived / all-failed sends leave no claim at all (pinned store-side).
 *
 * Mock harness mirrors `GroupChatPanel.toChip.test.tsx`.
 */
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT_A = "agent-a";
const ME = "user-me";
const GROUP = "group-1";

let tail: ChatMessageRow[];
// The claim the store holds for this session; `null` = nothing in flight.
let inflightMap: Record<string, unknown> = {};
// Registry `role` of the fan-out's agent. Mutable per case: the header paints
// it under the name only when the claim names ONE responder (ADR-106 rev.
// 2026-11-08), which is the rule the case below pins on this surface.
let agentRole: string | undefined;
let settleTurnMock: ReturnType<typeof vi.fn> = vi.fn();

vi.mock("@tauri-apps/api/core", () => ({ convertFileSrc: (x: string) => x }));

vi.mock("../../stores/chatSessionStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../stores/chatSessionStore")>();
  return {
    ...actual,
    useChatSessionStore: Object.assign(
      (selector: (state: unknown) => unknown) =>
        selector({
          windows: {
            [GROUP]: {
              older: [],
              tail,
              tailLoaded: tail.length,
              total: tail.length,
              loadingOlder: false,
            },
          },
          sending: {},
          inflight: inflightMap,
        }),
      {
        getState: () => ({
          setMode: vi.fn(),
          sendGroupText: vi.fn().mockResolvedValue(true),
          loadOlder: vi.fn().mockResolvedValue(undefined),
          upload: vi.fn().mockResolvedValue({ id: "a1", filename: "f", mime: "image/png", size: 1 }),
          settleTurn: (...a: unknown[]) => settleTurnMock(...(a as [])),
        }),
      },
    ),
  };
});

// The pushed `session_state` per execution session. The wait line's activity
// word reads exactly this, so a case that wants an agent to look busy sets it.
let agentStates: Record<string, unknown> = {};

vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector({ agentStates }),
    { getState: () => ({ openSession: vi.fn() }) },
  ),
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: Object.assign(
    (selector: (state: unknown) => unknown) =>
      selector({
        agents: {
          [AGENT_A]: { meta: { name: "Agent A", role: agentRole } },
        },
      }),
    { getState: () => ({ selectAgent: vi.fn() }) },
  ),
}));

vi.mock("../../stores/projectGroupStore", () => ({
  useProjectGroupStore: (selector: (state: unknown) => unknown) =>
    selector({
      activeGroupId: GROUP,
      groups: [
        {
          session_id: GROUP,
          kind: "project_group",
          title: "Group",
          participants: [
            { id: ME, identity: "human" },
            { id: AGENT_A, identity: "agent" },
          ],
        },
      ],
    }),
}));

vi.mock("../../stores/authStore", () => ({
  useAuthStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector({ account: { user_id: ME } }),
    { getState: () => ({ accessToken: "tok" }) },
  ),
}));

vi.mock("../../stores/userProfileStore", () => ({
  useUserProfileStore: (selector: (state: unknown) => unknown) =>
    selector({ profile: { displayName: "You", backendAvatarUrl: null, backendBuiltinAvatarId: null } }),
}));

vi.mock("../../lib/auth-api", () => ({
  fetchDirectory: vi.fn().mockResolvedValue([]),
}));
vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://127.0.0.1:1" }));
vi.mock("../../lib/stickToBottom", () => ({
  useStickToBottom: () => ({ ref: { current: null }, stickNow: vi.fn() }),
}));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string) => key,
    // The real hook returns `{ t, i18n }`; the turn header reads the locale to
    // join a multi-responder name chain, so the fake has to carry it.
    i18n: { language: "zh-CN" },
  }),
}));

vi.mock("../common/AgentAvatar", () => ({ AgentAvatar: () => <div /> }));
vi.mock("../common/UserAvatar", () => ({ UserAvatar: () => <div /> }));
vi.mock("../common/Tooltip", () => ({
  Tooltip: ({ children }: { children?: React.ReactNode }) => <>{children}</>,
}));
vi.mock("./CodeBlock", () => ({ CodeBlock: () => <div /> }));
vi.mock("../editor/markdownLinkResolver", () => ({
  handleChatMarkdownLinkClick: vi.fn(),
  markdownUrlTransform: (x: string) => x,
}));
// What a round contains and how it is going is the block's own contract
// (pinned in ChatExecutionBlock.test.tsx). Here it is a marker, so these cases
// stay about one thing: does the surface still claim the turn is its own send.
vi.mock("./ChatExecutionBlock", () => ({
  ChatExecutionBlock: () => <div data-testid="exec-block" />,
}));

function row(seq: number, over: Partial<ChatMessageRow>): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: ME,
    kind: "user_message",
    body: "hello",
    to: "",
    meta: { message_id: `m${seq}` },
    ...over,
  };
}

const EXEC = "exec-a1";

/** A round row the agent owns — what gives the surface a run to speak for. */
function roundRow(seq: number, execSid = EXEC): ChatMessageRow {
  return row(seq, {
    from: AGENT_A,
    kind: "execution_round",
    body: "",
    to: ME,
    meta: {
      message_id: `round:${execSid}|r${seq}`,
      agent_instance_id: AGENT_A,
      agent_session_id: execSid,
      round_id: `r${seq}`,
      in_reply_to: "m1",
    },
  });
}

/** The composer gate proper: the textarea is disabled by exactly one flag,
 *  `composerLocked`, so it separates the claim's lock from the button's other
 *  reason to sit out (an empty draft). */
function composerLocked(): boolean {
  return (screen.getByRole("textbox") as HTMLTextAreaElement).disabled;
}

/** The group's send button is also gated on having something to send, so a
 *  case that expects it live must type a draft first — otherwise the empty
 *  composer, not the claim, would be deciding the assertion. */
function sendDisabled(): boolean {
  return (
    screen.getByRole("button", { name: "groupChat.send" }) as HTMLButtonElement
  ).disabled;
}

function typeDraft() {
  fireEvent.change(screen.getByRole("textbox"), {
    target: { value: "keep going" },
  });
}

describe("GroupChatPanel in-flight turn claim", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    inflightMap = {};
    agentRole = undefined;
    agentStates = {};
    settleTurnMock = vi.fn();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  it("the claim past the POST holds the line up and locks the composer (D9)", () => {
    tail = [row(1, { body: "fix the pagination" })];
    inflightMap = { [GROUP]: { messageId: "m1", at: 1 } };
    render(<GroupChatPanel />);
    // Generic wording: the fan-out names no single agent.
    expect(screen.getByRole("status").textContent).toContain(
      "chatView.notice.pendingTurnGeneric",
    );
    expect(composerLocked()).toBe(true);
    expect(sendDisabled()).toBe(true);
  });

  it("a claim naming its responders wears the turn header (ADR-106 rev 2026-11-08)", () => {
    // The names come from the delivery decision's inputs recorded at send
    // time, not from the transcript's shape: with them the wait wears faces.
    tail = [row(1, { body: "fix the pagination" })];
    inflightMap = { [GROUP]: { messageId: "m1", at: 1, responders: [AGENT_A] } };
    render(<GroupChatPanel />);
    const header = document.querySelector("[data-turn-header]");
    expect(header).toBeTruthy();
    expect(header!.textContent).toContain("Agent A");
    // The centered line steps back — one statement, one shape.
    expect(document.querySelector("[data-event-text]")).toBeNull();
    expect(composerLocked()).toBe(true);
  });

  it("a single-responder claim carries that agent's role, as its block will", () => {
    // Same registry meta the block header reads: the wait line and the block
    // that takes over must agree on who is speaking, so the role is already
    // under the name when the swap happens.
    agentRole = "资深工程师";
    tail = [row(1, { body: "fix the pagination" })];
    inflightMap = { [GROUP]: { messageId: "m1", at: 1, responders: [AGENT_A] } };
    render(<GroupChatPanel />);
    expect(document.querySelector("[data-turn-header]")!.textContent).toContain("资深工程师");
  });

  it("an unresolvable responder degrades the header to the generic line", () => {
    // An agent the registry lost must not buy the header a blank face: the
    // surface falls back to the centered line it used before the revision.
    tail = [row(1, { body: "fix the pagination" })];
    inflightMap = { [GROUP]: { messageId: "m1", at: 1, responders: ["ghost-x"] } };
    render(<GroupChatPanel />);
    expect(document.querySelector("[data-turn-header]")).toBeNull();
    expect(screen.getByRole("status").textContent).toContain(
      "chatView.notice.pendingTurnGeneric",
    );
  });

  it("the round's reference row settles the claim: line gone, composer live", () => {
    // The service answered the human's row with a round; that fact is projected
    // onto the row as `delivery.answered` (design 31 §6), which is what the
    // wait line ends on.
    tail = [
      row(1, {
        body: "fix the pagination",
        delivery: {
          answered: true,
          answered_by: { message_id: "round:exec-1|r1", kind: "execution_round" },
          target: {
            agent_instance_id: AGENT_A,
            agent_session_id: "exec-1",
            round_id: "r1",
          },
        },
      }),
      row(2, {
        from: AGENT_A,
        kind: "execution_round",
        body: "",
        meta: { message_id: "round:exec-1|r1", round_id: "r1", in_reply_to: "m1" },
      }),
    ];
    inflightMap = { [GROUP]: { messageId: "m1", at: 1 } };
    render(<GroupChatPanel />);
    expect(screen.queryByRole("status")).toBeNull();
    expect(composerLocked()).toBe(false);
    typeDraft();
    expect(sendDisabled()).toBe(false);
    expect(settleTurnMock).toHaveBeenCalledWith(GROUP, "m1");
  });

  it("a live delivery alert settles it too — the human has an answer on screen", () => {
    // ADR-105 D5 rev. 2026-11-06: the failure no longer lands as a receipt row,
    // so the projection reports it on the human's own row. That IS the outcome
    // the wait line was waiting for — holding the claim would lock the composer
    // on a send that already bounced.
    tail = [
      row(1, {
        body: "fix the pagination",
        delivery: {
          answered: false,
          alert: { target: AGENT_A, error_code: "target_not_installed", expires_in: 600 },
        },
      }),
    ];
    inflightMap = { [GROUP]: { messageId: "m1", at: 1 } };
    render(<GroupChatPanel />);
    expect(screen.queryByRole("status")).toBeNull();
    expect(composerLocked()).toBe(false);
    typeDraft();
    expect(sendDisabled()).toBe(false);
  });

  it("no claim of our own, no line: an unanswered row is not liveness (D9)", () => {
    // The reload case. The newest human row has no answer and the surface knows
    // nothing else — claiming busy here is what made a dead node look alive
    // forever, and it would lock a composer with nothing behind it.
    tail = [row(1, { body: "fix the pagination" })];
    render(<GroupChatPanel />);
    expect(screen.queryByRole("status")).toBeNull();
    expect(composerLocked()).toBe(false);
    typeDraft();
    expect(sendDisabled()).toBe(false);
  });
});

describe("GroupChatPanel the wait line says what the run is doing", () => {
  // Rev. (2026-11-12, the activity word). The group shares the 1:1 surface's
  // map, and shares its restraint: the word comes from the run this Chat
  // anchors AND that the claim addressed. A fan-out is not described by one
  // member's tool call, and a turn the router has not placed yet has no run
  // to describe at all — those are the two windows where the generic sentence
  // is the honest one.
  const busyWith = (tools: string[], execSid = EXEC) => ({
    [AGENT_A]: {
      sessionStates: {
        [execSid]: {
          sessionStatus: { status: "tool_executing", detail: { tools } },
        },
      },
    },
  });

  it("a run the claim addressed names its work under the addressed face", () => {
    tail = [row(1, { body: "check what we decided last time" }), roundRow(2)];
    inflightMap = { [GROUP]: { messageId: "m1", at: 1, responders: [AGENT_A] } };
    agentStates = busyWith(["memory_recall"]);
    render(<GroupChatPanel />);
    const header = document.querySelector("[data-turn-header]");
    expect(header).toBeTruthy();
    expect(header!.textContent).toContain("tools.memory_recall.running");
    expect(header!.textContent).not.toContain("chatView.notice.pendingTurnGeneric");
  });

  it("before the first round lands there is no run to speak for", () => {
    // The router-deciding window: the session this turn goes to may not exist
    // yet, and the previous task's session is a different conversation's work.
    tail = [row(1, { body: "check what we decided last time" })];
    inflightMap = { [GROUP]: { messageId: "m1", at: 1, responders: [AGENT_A] } };
    agentStates = busyWith(["memory_recall"]);
    render(<GroupChatPanel />);
    const header = document.querySelector("[data-turn-header]");
    expect(header!.textContent).toContain("chatView.notice.pendingTurnGeneric");
    expect(header!.textContent).not.toContain("tools.memory_recall.running");
  });

  it("a run the claim did not address is not this turn's activity", () => {
    // Same transcript, same busy session — but the send carries no responder
    // set, so the surface has no warrant to say who this turn went to. The
    // generic line is what the claim can actually support.
    tail = [row(1, { body: "check what we decided last time" }), roundRow(2)];
    inflightMap = { [GROUP]: { messageId: "m1", at: 1 } };
    agentStates = busyWith(["memory_recall"]);
    render(<GroupChatPanel />);
    expect(screen.getByRole("status").textContent).toContain(
      "chatView.notice.pendingTurnGeneric",
    );
    expect(screen.getByRole("status").textContent).not.toContain(
      "tools.memory_recall.running",
    );
  });

  it("an idle session does not get a work word", () => {
    tail = [row(1, { body: "check what we decided last time" }), roundRow(2)];
    inflightMap = { [GROUP]: { messageId: "m1", at: 1, responders: [AGENT_A] } };
    agentStates = {
      [AGENT_A]: { sessionStates: { [EXEC]: { sessionStatus: { status: "idle" } } } },
    };
    render(<GroupChatPanel />);
    const header = document.querySelector("[data-turn-header]");
    expect(header!.textContent).toContain("chatView.notice.pendingTurnGeneric");
  });
});
