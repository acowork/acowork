/**
 * The group surface docks the router's question at the bottom, too.
 *
 * ADR-106 revision 2026-11-11: the question about a run is parked over the
 * composer's edge — the run it hangs on is anchored where that exchange
 * STARTED, so a question drawn at the run's tail is a question the reader
 * never sees. What this file pins is the GROUP host's wiring: the dock
 * appears above the composer, and the run is told its question is docked so
 * its tail stays empty (one question, one place). The card's own behaviour and
 * the answer path are pinned in the 1:1 surface's suite — the two hosts share
 * the same component and the same handler.
 */
import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatDeliveryView, ChatMessageRow } from "../../stores/chatSessionStore";
import type { RunBlock } from "./chatTranscriptGroups";

const AGENT = "agent-1";
const ME = "user-me";
const GROUP = "group-1";

/** Rows fed to the mocked message window; each test overwrites this. */
let tail: ChatMessageRow[];
/** The group's roster — the read/write split (a non-participant may read but
 *  not answer). */
let participants: Array<{ id: string; identity: string }>;

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
          sending: { [GROUP]: false },
          inflight: {},
        }),
      {
        getState: () => ({
          sendGroupText: vi.fn().mockResolvedValue(true),
          loadOlder: vi.fn().mockResolvedValue(undefined),
          upload: vi
            .fn()
            .mockResolvedValue({ id: "a1", filename: "f", mime: "image/png", size: 1 }),
        }),
      },
    ),
  };
});

vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector({ agentStates: {} }),
    { getState: () => ({ openSession: vi.fn(), resolveQuestion: vi.fn() }) },
  ),
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: Object.assign(
    (selector: (state: unknown) => unknown) =>
      selector({ agents: { [AGENT]: { meta: { name: "Agent A" } } } }),
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
          title: "Group",
          participants,
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

// The block itself is pinned in ChatExecutionBlock.test.tsx; here it only has to
// say whether the surface docked its question.
vi.mock("./ChatExecutionBlock", () => ({
  ChatExecutionBlock: (props: {
    block: RunBlock;
    pendingAskDocked?: boolean;
  }) => (
    <div
      data-testid="exec-block"
      data-session={`${props.block.instanceId}|${props.block.execSessionId}`}
      data-docked={props.pendingAskDocked ? "yes" : "no"}
    />
  ),
}));

/** One round's pointer row (D3): bodyless, and it names a run. */
function roundRow(seq: number): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: AGENT,
    kind: "execution_round",
    body: "",
    to: ME,
    meta: {
      message_id: `round:exec-7|r${seq}`,
      agent_instance_id: AGENT,
      agent_session_id: "exec-7",
      round_id: `r${seq}`,
      in_reply_to: "u1",
    },
  };
}

/** What the service projects onto an addressing card (design 31 §6). */
function cardDelivery(options: Array<{ session_id: string; label?: string }>): ChatDeliveryView {
  return {
    answered: false,
    question: { answered: false, options, allow_new_task: true },
  };
}

describe("GroupChatPanel — the router's question is docked", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    participants = [
      { id: ME, identity: "human" },
      { id: AGENT, identity: "agent" },
    ];
  });

  it("parks the run's question over the composer and tells the run it is docked", () => {
    tail = [
      {
        seq: 1,
        ts: 1,
        from: ME,
        kind: "user_message",
        body: "继续刚才那个",
        to: AGENT,
        meta: { message_id: "u1" },
      },
      roundRow(2),
      {
        seq: 3,
        ts: 3,
        from: AGENT,
        kind: "system_notice",
        body: "",
        to: ME,
        meta: { message_id: "n3", notice_type: "confirm_continue" },
        delivery: cardDelivery([{ session_id: "exec-7", label: "Fix the bug" }]),
      },
    ];
    render(<GroupChatPanel />);
    const dock = screen.getByTestId("chat-ask-dock");
    expect(dock.textContent).toContain("chatView.block.confirmSession");
    expect(dock.querySelector('[data-testid="chat-card-options"]')).not.toBeNull();
    // The run is the question's subject and knows the question is docked.
    expect(screen.getByTestId("exec-block").getAttribute("data-docked")).toBe("yes");
  });

  it("shows the question to a non-participant, but lets them answer nothing", () => {
    // The group surface's read/write split: a viewer outside the roster may
    // read the question, never decide it. The dock must not hand a non-
    // participant the buttons the blocks withhold.
    participants = [{ id: AGENT, identity: "agent" }];
    tail = [
      {
        seq: 1,
        ts: 1,
        from: ME,
        kind: "user_message",
        body: "继续刚才那个",
        to: AGENT,
        meta: { message_id: "u1" },
      },
      roundRow(2),
      {
        seq: 3,
        ts: 3,
        from: AGENT,
        kind: "system_notice",
        body: "",
        to: ME,
        meta: { message_id: "n3", notice_type: "confirm_continue" },
        delivery: cardDelivery([{ session_id: "exec-7", label: "Fix the bug" }]),
      },
    ];
    render(<GroupChatPanel />);
    const dock = screen.getByTestId("chat-ask-dock");
    expect(dock.textContent).toContain("chatView.block.confirmSession");
    expect(dock.textContent).toContain("chatView.block.answeredByOwner");
    // The candidate is a fact, not a button this viewer may press.
    expect(screen.queryByRole("button", { name: /Fix the bug/ })).toBeNull();
  });
});
