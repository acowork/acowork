// @vitest-environment jsdom
/**
 * The PM dispatch row renders a task chip (TaskChip) above the human
 * text, driven solely by `meta.task_id` — the playbook that wakes the
 * agent never lands on this row. Two real defects this pins:
 *   1. the chip must appear when the storage boundary delivers
 *      `meta.task_id` (it was silently dropped by `IntentMeta` before
 *      Commit 1 — the producer/consumer pair passed their own tests while
 *      the feature never rendered);
 *   2. it must NOT appear on rows that merely mention a task in text, or
 *      on agent rows: only the dispatch row owns the coordinate.
 *
 * Mock harness mirrors `GroupChatPanel.waitLine.test.tsx`.
 */
import { render, screen, fireEvent } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

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
      selector({ agents: { [AGENT]: { meta: { display_name: "Agent A", name: "agent-a" } } } }),
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
// Echo the key (and the interpolated id) so assertions see both.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, opts?: Record<string, unknown>) =>
      opts && "id" in opts ? `${key}:${opts.id}` : key,
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

function dispatchRow(seq: number, taskId?: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: ME,
    kind: "user_message",
    body: "修复分页不满屏",
    to: AGENT,
    meta: {
      message_id: `u${seq}`,
      conversation_id: "task:p-1:t-083ae242",
      ...(taskId ? { task_id: taskId } : {}),
    },
  };
}

describe("GroupChatPanel task chip", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText: vi.fn().mockResolvedValue(undefined) },
    });
  });

  it("renders the chip with the task id when the dispatch row carries meta.task_id", () => {
    tail = [dispatchRow(1, "t-083ae242")];
    render(<GroupChatPanel />);
    expect(screen.getByText("groupChat.taskChip:t-083ae242")).toBeTruthy();
  });

  it("renders no chip on a plain human row", () => {
    tail = [dispatchRow(1)];
    render(<GroupChatPanel />);
    expect(screen.queryByText(/groupChat\.taskChip/)).toBeNull();
  });

  it("renders no chip on a non-user row even if meta carries a task_id", () => {
    tail = [
      {
        ...dispatchRow(1, "t-083ae242"),
        from: AGENT,
        kind: "agent_message",
        meta: { message_id: "a1", task_id: "t-083ae242" },
      },
    ];
    render(<GroupChatPanel />);
    expect(screen.queryByText(/groupChat\.taskChip/)).toBeNull();
  });

  it("copies the bare task id on click", async () => {
    tail = [dispatchRow(1, "t-083ae242")];
    render(<GroupChatPanel />);
    fireEvent.click(screen.getByRole("button", { name: /groupChat\.taskChip/ }));
    expect(navigator.clipboard.writeText).toHaveBeenCalledWith("t-083ae242");
    // Flush the async copied-state update (the checkmark) so the promise
    // continuation lands inside React's act loop, not after the test.
    await screen.findByText("✓");
  });
});
