/**
 * ADR-100 D12: a PM task session is bound to this group's Chat session, so a
 * turn composed in its execution view arrives as a `user_message` whose
 * attached items are labels (the file belongs to the run, not to the chat —
 * design 27 §4.2.1). Three things must hold on the group surface: the labels
 * paint, they count as content (the empty-turn placeholder is what a "here's a
 * file, go" message would otherwise degrade to), and — since D12 Rev. 2026-11
 * — a workspace arm sent by an Agent is openable against **that Agent's**
 * workspaces.
 *
 * Mock harness mirrors `GroupChatPanel.exec.test.tsx`.
 */
import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";
import { openAttachedRef } from "../../lib/openWorkspaceRef";

vi.mock("../../lib/openWorkspaceRef", () => ({ openAttachedRef: vi.fn() }));

const AGENT = "agent-1";
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
          sending: { [GROUP]: false },
          inflight: {},
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
          participants: [
            { id: ME, identity: "human" },
            { id: AGENT, identity: "agent" },
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

function pinnedRow(
  seq: number,
  body: string,
  labels: Array<{ filename: string; kind?: string; absPath?: string }>,
  from: string = ME,
): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from,
    kind: "user_message",
    body,
    to: "",
    meta: { message_id: `u${seq}`, exec_attachments: labels },
  };
}

describe("GroupChatPanel execution-attachment labels", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  it("paints the labels of a pinned turn sent with words", () => {
    tail = [pinnedRow(1, "review this", [{ filename: "src/main.rs", kind: "file" }])];
    render(<GroupChatPanel />);
    expect(screen.getByText("src/main.rs")).toBeTruthy();
    expect(screen.getByText("review this")).toBeTruthy();
  });

  it("suppresses the empty-turn placeholder when the turn was a file and nothing else", () => {
    tail = [pinnedRow(1, "", [{ filename: "diff.patch", kind: "upload" }])];
    render(<GroupChatPanel />);
    expect(screen.getByText("diff.patch")).toBeTruthy();
    expect(screen.queryByText("groupChat.emptyTurn")).toBeNull();
  });

  it("still shows the placeholder for a genuinely empty row", () => {
    tail = [pinnedRow(1, "", [])];
    render(<GroupChatPanel />);
    expect(screen.getByText("groupChat.emptyTurn")).toBeTruthy();
  });

  it("opens a workspace arm against the Agent that pointed at it", () => {
    // The path resolves in the sender's own workspace registry — which on this
    // surface is the Agent participant's, not "whichever agent happens to be
    // selected elsewhere".
    tail = [
      pinnedRow(
        1,
        "look at this",
        [{ filename: "src/main.rs", kind: "attached_file", absPath: "E:/ws/src/main.rs" }],
        AGENT,
      ),
    ];
    render(<GroupChatPanel />);
    screen.getByRole("button", { name: "src/main.rs" }).click();
    expect(openAttachedRef).toHaveBeenCalledWith({
      item: { type: "attached_file", absPath: "E:/ws/src/main.rs", name: "src/main.rs" },
      agentId: AGENT,
      currentWorkspaceId: "",
    });
  });

  it("leaves a human's own workspace arm decorative", () => {
    // This surface is not bound to one Agent's workspace registry, so there is
    // nothing honest to resolve a person's path against — a click that guesses
    // would open the wrong tree. The chip paints, and does not pretend.
    tail = [
      pinnedRow(1, "look at this", [
        { filename: "src/main.rs", kind: "attached_file", absPath: "E:/ws/src/main.rs" },
      ]),
    ];
    render(<GroupChatPanel />);
    expect(screen.getByText("src/main.rs")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "src/main.rs" })).toBeNull();
    expect(openAttachedRef).not.toHaveBeenCalled();
  });
});
