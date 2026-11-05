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

vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector({ agentStates: {} }),
    { getState: () => ({ openSession: vi.fn() }) },
  ),
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: Object.assign(
    (selector: (state: unknown) => unknown) =>
      selector({
        agents: { [AGENT_A]: { meta: { display_name: "Agent A", name: "agent-a" } } },
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
  useTranslation: () => ({ t: (key: string) => key }),
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
