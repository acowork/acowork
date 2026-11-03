// @vitest-environment jsdom
/**
 * The task coordinate on a round's block header (ADR-106 D13).
 *
 * The PM dispatch used to wear its own chip on the human row. `task_id` now
 * rides the `execution_round` meta, because the round — not the utterance — is
 * the unit that belongs to a task, and the block header is the one place a
 * reader looks for "which piece of work is this". Two things this pins:
 *   1. the id renders when the row carries it (the producer/consumer pair each
 *      passed their own tests while the feature never rendered once);
 *   2. nothing paints when the row carries none — a header that invented a task
 *      coordinate would be writing history the transcript does not hold.
 */
import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";
import type { ChatBlockEntry } from "../../lib/chatBlocks";

const AGENT = "agent-1";
const ME = "user-me";
const GROUP = "group-1";

let tail: ChatMessageRow[];
let blockEntry: ChatBlockEntry | null;

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

vi.mock("../../stores/chatBlockStore", () => ({
  // The block reads its whole session at once (ADR-106 D4): a record keyed by
  // ref, which is what the component indexes into.
  useChatBlocks: () => (blockEntry ? { [blockEntry.ref]: blockEntry } : {}),
  useChatBlockStore: (selector: (s: { reload: () => void }) => unknown) =>
    selector({ reload: vi.fn() }),
}));

vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector({ agentStates: {} }),
    { getState: () => ({ openSession: vi.fn(), resolveQuestion: vi.fn() }) },
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

function roundRow(taskId?: string): ChatMessageRow {
  return {
    seq: 2,
    ts: 2,
    from: AGENT,
    kind: "execution_round",
    body: "",
    to: ME,
    meta: {
      message_id: "round:exec-1|r2",
      agent_instance_id: AGENT,
      agent_session_id: "exec-1",
      round_id: "r2",
      in_reply_to: "u1",
      ...(taskId ? { task_id: taskId } : {}),
    },
  };
}

function humanRow(taskId?: string): ChatMessageRow {
  return {
    seq: 1,
    ts: 1,
    from: ME,
    kind: "user_message",
    body: "修复分页不满屏",
    to: AGENT,
    meta: {
      message_id: "u1",
      conversation_id: "task:p-1:t-083ae242",
      ...(taskId ? { task_id: taskId } : {}),
    },
  };
}

function entryOf(): ChatBlockEntry {
  return {
    ref: `${AGENT}|exec-1|r2`,
    agent_instance_id: AGENT,
    agent_session_id: "exec-1",
    round_id: "r2",
    status: "ok",
    projection: {
      round_id: "r2",
      terminal: true,
      status: "completed",
      answer: { body: "done", ts: "1" },
    },
  };
}

describe("GroupChatPanel round task coordinate", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
    blockEntry = entryOf();
  });

  it("states the task id in the block header when the round carries meta.task_id", () => {
    tail = [humanRow(), roundRow("t-083ae242")];
    render(<GroupChatPanel />);
    expect(screen.getByText("t-083ae242")).toBeTruthy();
  });

  it("invents no task coordinate for a round that carries none", () => {
    tail = [humanRow(), roundRow()];
    render(<GroupChatPanel />);
    expect(screen.queryByText(/t-083ae242/)).toBeNull();
  });

  it("keeps the human dispatch row free of the coordinate (D13: it rides the round)", () => {
    // The dispatch still carries its `conversation_id`, which is the task's
    // conversation, not a chip on the utterance.
    tail = [humanRow("t-083ae242"), roundRow()];
    render(<GroupChatPanel />);
    expect(screen.queryByText(/t-083ae242/)).toBeNull();
  });
});
