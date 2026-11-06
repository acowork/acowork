/**
 * A PM dispatch in the project group chat: what the reader must still get.
 *
 * ADR-106 D12 removed the dispatch's `user_message` row on purpose — the
 * dispatch IS the round's first user row, read back through the projection, so
 * the Chat does not keep a second copy of a fact it does not own. D13 moved the
 * task coordinate from the human row onto the round, and the block header is
 * where it paints.
 *
 * Measured consequence of the two decisions landing without their second half:
 * the group showed a block named "unnamed run" beside its task chip and nothing
 * else — no title anywhere — so the dispatch read as if the human's message had
 * been swallowed. Worse, the reference row carried the PM wake-up PLAYBOOK in
 * `meta.assignment_brief` (design 27 §5.4.2: never persisted, never painted), so
 * the group's list preview and an unread projection printed the claim/submit
 * instructions.
 *
 * This pins the pair the reader is owed:
 *   1. one block, wearing the task id;
 *   2. named by the dispatch text (no trigger row exists to name it), so the
 *      human's words are on the transcript at the block — without a second
 *      bubble, and without the playbook.
 */
import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";
import type { ChatBlockEntry } from "../../lib/chatBlocks";

const AGENT = "agent-1";
const ME = "user-me";
const GROUP = "group-1";
const TASK = "t-1b7bdb21";
const TITLE = "修复分页不满屏";
const DESCRIPTION = "列表页翻到最后一页时不铺满屏幕";
const PLAYBOOK =
  'You are being woken to execute PM task "' + TASK + '". Follow this workflow exactly:';

let tail: ChatMessageRow[];
let blocks: Record<string, ChatBlockEntry>;

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
  useChatBlocks: () => blocks,
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

/** The dispatch round exactly as the store holds it: no `user_message` row
 *  anywhere (D12), `in_reply_to` naming the PM dispatch id that no chat row
 *  carries, the task coordinate, and `opened_new` (the dispatch opened the
 *  session). `brief` is the ROW's name — the dispatch text; `legacyBrief` is
 *  what rows written before the producer fix carry: the playbook. */
function dispatchRound(seq: number, opts: { legacyBrief?: boolean } = {}): ChatMessageRow {
  const roundId = `r:pm-${seq}`;
  return {
    seq,
    ts: seq,
    from: AGENT,
    kind: "execution_round",
    body: "",
    to: ME,
    meta: {
      message_id: `round:${roundId}`,
      agent_instance_id: AGENT,
      agent_session_id: "exec-1",
      round_id: roundId,
      in_reply_to: `pm-${seq}`,
      opened_new: true,
      task_id: TASK,
      assignment_brief: opts.legacyBrief ? PLAYBOOK : `${TITLE}\n${DESCRIPTION}`,
    },
  };
}

function entryOf(roundId: string, question: string | null): ChatBlockEntry {
  return {
    ref: `${AGENT}|exec-1|${roundId}`,
    agent_instance_id: AGENT,
    agent_session_id: "exec-1",
    round_id: roundId,
    status: "ok",
    projection: {
      round_id: roundId,
      terminal: true,
      status: "completed",
      ...(question === null ? {} : { user: { body: question, ts: "1" } }),
      answer: { body: "done", ts: "1" },
    },
  };
}

describe("GroupChatPanel: a PM dispatch in the project group", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  it("names the block with the dispatch text and wears the task id", () => {
    // The transcript holds ONE row — the reference row. The human's words reach
    // the surface through the projection, which is what D12 said they would.
    tail = [dispatchRound(1)];
    blocks = { [`${AGENT}|exec-1|r:pm-1`]: entryOf("r:pm-1", `${TITLE}\n${DESCRIPTION}`) };
    render(<GroupChatPanel />);
    // The block exists and carries the task coordinate (D13's chip).
    expect(screen.getByText(TASK)).toBeTruthy();
    // The dispatch text is on screen — as the block's name and as the round's
    // line — so the reader is not left with an anonymous block.
    expect(screen.getAllByText(new RegExp(TITLE)).length).toBeGreaterThan(0);
    expect(screen.getAllByText(new RegExp(DESCRIPTION)).length).toBeGreaterThan(0);
    // No user bubble was invented beside it: the words stand at the block, and
    // the run is named by them, so the "question → answer" pairing survives the
    // removed chat row without a second copy of the sentence.
    expect(screen.queryByText("You")).toBeNull();
    expect(screen.queryByText("chatView.block.unnamedRun")).toBeNull();
  });

  it("never paints the PM playbook as the round's question", () => {
    // A row written before `reference_brief` carries the wake-up playbook in
    // its `assignment_brief`, and this projection could not be read. Design 27
    // §5.4.2: the playbook reaches the system block and no chat surface.
    tail = [dispatchRound(1, { legacyBrief: true })];
    blocks = { [`${AGENT}|exec-1|r:pm-1`]: entryOf("r:pm-1", null) };
    render(<GroupChatPanel />);
    expect(screen.getByText(TASK)).toBeTruthy();
    expect(screen.queryByText(/being woken to execute/)).toBeNull();
    expect(screen.queryByText(/Claim it first/)).toBeNull();
  });

  it("keeps the block identifiable when the projection cannot name it", () => {
    // A block whose projection read failed has no human text to be named by.
    // It must still say WHICH piece of work it is (the durable task chip) and
    // must not paper over the hole with the reference row's string — that
    // string is a durable name whose meaning changed between builds, which is
    // exactly how the playbook reached the header.
    tail = [dispatchRound(1, { legacyBrief: true })];
    blocks = { [`${AGENT}|exec-1|r:pm-1`]: entryOf("r:pm-1", null) };
    render(<GroupChatPanel />);
    expect(screen.getByText(TASK)).toBeTruthy();
    expect(screen.queryByText(/being woken to execute/)).toBeNull();
  });
});
