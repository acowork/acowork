// @vitest-environment jsdom
/**
 * Every group row bound to an execution session paints a "TO <name>" capsule
 * naming its counterparty, so the reader can pair a user message with the
 * agent it reaches (and an agent reply with the initiator it answers) without
 * digging through history. The target is read straight off the durable row:
 *   1. `user_message` → `to` (PM dispatch, pinned execution-session turn);
 *   2. an @-fan-out broadcast leaves `to` empty and lists the woken agents in
 *      `meta.mentions` → one capsule per target;
 *   3. `agent_message` → `to` (the initiator it answers);
 *   4. no address (empty `to`, no mentions) → no capsule.
 *
 * Mock harness mirrors `GroupChatPanel.taskChip.test.tsx`.
 */
import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT_A = "agent-a";
const AGENT_B = "agent-b";
const ME = "user-me";
const GROUP = "group-1";

let tail: ChatMessageRow[];

vi.mock("@tauri-apps/api/core", () => ({ convertFileSrc: (x: string) => x }));

vi.mock("../../stores/chatSessionStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../stores/chatSessionStore")>();
  return {
    ...actual,
    useChatSessionStore: Object.assign(
      (selector: (state: unknown) => unknown) =>
        selector({
          windows: {
            [GROUP]: { older: [], tail, tailLoaded: tail.length, total: tail.length, loadingOlder: false },
          },
          sending: {},
        }),
      {
        getState: () => ({
          setMode: vi.fn(),
          sendGroupText: vi.fn().mockResolvedValue(true),
          loadOlder: vi.fn().mockResolvedValue(undefined),
          upload: vi.fn().mockResolvedValue({ id: "a1", filename: "f", mime: "image/png", size: 1 }),
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
        agents: {
          [AGENT_A]: { meta: { display_name: "Agent A", name: "agent-a" } },
          [AGENT_B]: { meta: { display_name: "Agent B", name: "agent-b" } },
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
            { id: AGENT_B, identity: "agent" },
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
  // The human participant resolves through the account directory, same as prod.
  fetchDirectory: vi
    .fn()
    .mockResolvedValue([{ user_id: "user-me", username: "you", display_name: "You" }]),
}));
vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://127.0.0.1:1" }));
vi.mock("../../lib/stickToBottom", () => ({
  useStickToBottom: () => ({ ref: { current: null }, stickNow: vi.fn() }),
}));
// Echo the key and the interpolated name so assertions see both.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, opts?: Record<string, unknown>) =>
      opts && "name" in opts ? `${key}:${opts.name}` : key,
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

describe("GroupChatPanel TO capsule", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  it("user_message addresses the target in `to` (PM dispatch / pinned turn)", () => {
    tail = [row(1, { to: AGENT_A })];
    render(<GroupChatPanel />);
    expect(screen.getByText("groupChat.toChip")).toBeTruthy();
    expect(screen.getByText("Agent A")).toBeTruthy();
  });

  it("@-fan-out broadcast shows one capsule per mentioned agent", () => {
    tail = [row(1, { to: "", meta: { message_id: "m1", mentions: [AGENT_A, AGENT_B] } })];
    render(<GroupChatPanel />);
    expect(screen.getByText("Agent A")).toBeTruthy();
    expect(screen.getByText("Agent B")).toBeTruthy();
    expect(screen.getAllByText("groupChat.toChip").length).toBe(2);
  });

  it("agent_message addresses the initiator it answers in `to`", () => {
    tail = [row(1, { from: AGENT_A, kind: "agent_message", to: AGENT_B })];
    render(<GroupChatPanel />);
    expect(screen.getByText("Agent B")).toBeTruthy();
  });

  it("agent_message addresses me by my profile name, not my raw id", () => {
    // The directory endpoint excludes the caller, so a reply addressed to me
    // must resolve through the live profile, never the uuid fallback.
    tail = [row(1, { from: AGENT_A, kind: "agent_message", to: ME })];
    render(<GroupChatPanel />);
    expect(screen.getByText("You")).toBeTruthy();
  });

  it("execution-session echo row (empty `to`) names the trigger's sender", () => {
    tail = [
      row(1, { from: ME, kind: "user_message", to: AGENT_A, meta: { message_id: "u1" } }),
      row(2, {
        from: AGENT_A,
        kind: "agent_message",
        to: "",
        body: "answer",
        meta: { message_id: "exec:a1", in_reply_to: "u1" },
      }),
    ];
    render(<GroupChatPanel />);
    // The echo row's chip resolves the trigger (u1)'s sender — me — by name.
    expect(screen.getAllByText("You").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("groupChat.toChip").length).toBeGreaterThanOrEqual(2);
  });

  it("paints no capsule when the row carries no address", () => {
    tail = [row(1, { to: "" })];
    render(<GroupChatPanel />);
    expect(screen.queryByText(/groupChat\.toChip/)).toBeNull();
  });
});