/**
 * A PM dispatch in the project group chat: the pair the reader is owed.
 *
 * ADR-106 D12 (rev. 2026-11-06) makes a dispatch **isomorphic to a 1:1 send**:
 * one `user_message` row in the project group plus the round's reference row
 * threaded under it — persisted as a pair, and entering the chat session's
 * context as a pair. The rev undid the original D12 wording, which dropped the
 * message row on the grounds that "the projection makes it visible": that held
 * for body visibility only, and it left the reference row's `in_reply_to`
 * naming a mailbox id no chat holds — no trigger row, no sentence in the
 * transcript (unread, in-chat search and the list preview all blind), and a
 * block that could be named only by a projection read.
 *
 * Two states have to stay readable, and this file pins both:
 *   - a paired dispatch (what every dispatch looks like from now on): the
 *     author's words stand as their own bubble, the block hangs under it wearing
 *     the task id, and the sentence is not painted twice;
 *   - a row written before the rev (no message row, `in_reply_to` dangling): the
 *     block must still be identifiable and must never borrow the wake-up
 *     playbook — design 27 §5.4.2, that string reaches the system block and no
 *     chat surface.
 *
 * And one rendering fact, which the dispatch row is the only `user_message`
 * that has: its words are the PM's generated Markdown, spoken in the author's
 * name — nobody typed them, so they render as Markdown, in the neutral speech
 * bubble. A person's own message stays literal text (`GroupChatPanel
 * .userLiteral`), and the dispatch's own `meta.task_id` is the marker that
 * tells the two apart.
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

/** The dispatch's own chat row — the task author's line in the group, carrying
 *  the task's text and the chip coordinate. */
function dispatchMessage(seq: number, from = ME): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from,
    kind: "user_message",
    body: `${TITLE}\n${DESCRIPTION}`,
    to: AGENT,
    meta: { message_id: `pm-${seq}`, task_id: TASK },
  };
}

/** The same dispatch, with the Markdown the PM actually generates: headings and
 *  emphasis in the body, which nobody typed. */
function dispatchMessageWithMarkdown(seq: number, from = ME): ChatMessageRow {
  return {
    ...dispatchMessage(seq, from),
    body: "## 任务\n\n1. 定位组件\n2. **第二步**",
  };
}

/** The round's reference row: contentless, threaded under the dispatch row it
 *  answers (`triggerId`) — or, for a row written before the rev, under a mailbox
 *  id no chat row carries. `legacyBrief` is what those older rows put in
 *  `round_name`: the wake-up playbook. */
function dispatchRound(
  seq: number,
  opts: { legacyBrief?: boolean; triggerId?: string } = {},
): ChatMessageRow {
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
      in_reply_to: opts.triggerId ?? `pm-${seq}`,
      opened_new: true,
      task_id: TASK,
      round_name: opts.legacyBrief ? PLAYBOOK : `${TITLE}\n${DESCRIPTION}`,
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

  it("stands the dispatch on the transcript, with the block under it", () => {
    // The pair (D12 rev.): the author's row + the round threaded under it. This
    // is what makes the group read as "who asked whom to do what" — and what
    // puts that pair into the chat session's context, because the row is a real
    // `user_message` and the reference row names it.
    tail = [dispatchMessage(1), dispatchRound(2, { triggerId: "pm-1" })];
    blocks = { [`${AGENT}|exec-1|r:pm-2`]: entryOf("r:pm-2", `${TITLE}\n${DESCRIPTION}`) };
    render(<GroupChatPanel />);
    // 1. The message is a message: the author's bubble, in the transcript.
    expect(screen.getByText("You")).toBeTruthy();
    expect(screen.getAllByText(new RegExp(DESCRIPTION)).length).toBeGreaterThan(0);
    // 2. The work hangs under it and keeps its task coordinate.
    expect(screen.getByText(TASK)).toBeTruthy();
    expect(screen.queryByText("chatView.block.unnamedRun")).toBeNull();
    // 3. One sentence, one bubble: an opener round whose words already stand on
    //    the transcript does not repeat them inside the block.
    expect(
      screen.getAllByText(new RegExp(DESCRIPTION)).filter((el) => el.tagName === "PRE"),
    ).toHaveLength(0);
  });

  it("renders the dispatch's generated Markdown as Markdown, in the speech skin", () => {
    // The dispatch body is the PM's own Markdown — the task title and
    // description it wrote — carried in the author's name. Nobody typed those
    // characters, so the row must not paint its own syntax as text. The
    // marker is the dispatch's own (`meta.task_id`, set by the Gateway's
    // dispatch path and by nothing a person sends), and the skin follows the
    // renderer: markdown wears the neutral bubble, because the prose palette
    // is painted for that backdrop.
    tail = [dispatchMessageWithMarkdown(1)];
    const { container } = render(<GroupChatPanel />);

    const bubble = container.querySelector(".bg-chat-bubble");
    expect(bubble, "the dispatch bubble must render").not.toBeNull();
    expect(bubble!.querySelector(".prose"), "the dispatch body must be parsed").not.toBeNull();
    expect(bubble!.querySelector("h2")?.textContent).toContain("任务");
    expect(bubble!.querySelector("strong")?.textContent).toContain("第二步");
    expect(container.textContent).not.toContain("## 任务");
    // Not the accent bubble: markdown in the accent one is the "user bubble
    // text turned black" defect, one level up.
    expect(container.querySelector(".bg-chat-user")).toBeNull();
  });

  it("still names a pre-rev dispatch block by the projection (no message row exists)", () => {
    // A dispatch sent before the rev left no `user_message` row, so the
    // transcript holds only the reference row and the projection is the only
    // place its words survive.
    tail = [dispatchRound(1)];
    blocks = { [`${AGENT}|exec-1|r:pm-1`]: entryOf("r:pm-1", `${TITLE}\n${DESCRIPTION}`) };
    render(<GroupChatPanel />);
    expect(screen.getByText(TASK)).toBeTruthy();
    expect(screen.getAllByText(new RegExp(TITLE)).length).toBeGreaterThan(0);
    expect(screen.getAllByText(new RegExp(DESCRIPTION)).length).toBeGreaterThan(0);
    expect(screen.queryByText("You")).toBeNull();
    expect(screen.queryByText("chatView.block.unnamedRun")).toBeNull();
  });

  it("never paints the PM playbook as the round's question", () => {
    // A row written before `reference_brief` carries the wake-up playbook in
    // its `round_name`, and this projection could not be read. Design 27
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
