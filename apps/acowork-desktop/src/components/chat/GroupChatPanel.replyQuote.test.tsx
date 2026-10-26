// @vitest-environment jsdom
/**
 * The group surface's answer quote (ReplyQuote).
 *
 * Design 27 §5.5.4 records a card answer on the answer row: the human's
 * `user_message` carries `in_reply_to` = the card's message id. The answered
 * card used to paint a full receipt shell repeating the question; that is gone,
 * and this file pins what replaced it on the group side — the same two entry
 * points the desktop chat has (the 1:1 twin is covered in
 * AgentChatView.test.tsx).
 *
 * Three things worth pinning separately, because each fails on its own:
 *   1. an answered card paints nothing, and the answer bubble quotes the
 *      question — the link must survive the receipt's removal;
 *   2. the quote resolves through a CARD-ONLY index. `in_reply_to` also points
 *      at plain messages, and quoting one of those would paint a decision quote
 *      where no decision was asked;
 *   3. a card that closed with nobody answering still leaves its quiet line —
 *      that fact exists nowhere else in the transcript.
 *
 * Mock harness mirrors `GroupChatPanel.taskChip.test.tsx`.
 */
import { render, screen } from "@testing-library/react";
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

vi.mock("../../lib/auth-api", () => ({ fetchDirectory: vi.fn().mockResolvedValue([]) }));
vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://127.0.0.1:1" }));
vi.mock("../../lib/stickToBottom", () => ({
  useStickToBottom: () => ({ ref: { current: null }, stickNow: vi.fn() }),
}));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, opts?: Record<string, unknown>) =>
      opts && "question" in opts
        ? `${key}«${opts.question}»`
        : opts && "answer" in opts
          ? `${key}«${opts.answer}»`
          : key,
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

function askCard(seq: number, messageId: string, question: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: AGENT,
    kind: "interaction_request",
    body: question,
    to: ME,
    meta: {
      message_id: messageId,
      // The runtime stamps the card with its trigger row (`in_reply_to` = the
      // human message that opened the turn) — that is what lets a terminal
      // agent row close it later. Omit it and `closedCardIds` cannot see the
      // closure at all.
      in_reply_to: "u1",
      agent_session_id: "exec-9",
      interaction: { kind: "ask_question", request_id: `q${seq}`, question },
    },
  };
}

function answerRow(seq: number, body: string, inReplyTo: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: ME,
    kind: "user_message",
    body,
    to: AGENT,
    meta: { message_id: `u${seq}`, in_reply_to: inReplyTo },
  };
}

function plainUserRow(seq: number, body: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: ME,
    kind: "user_message",
    body,
    to: AGENT,
    meta: { message_id: `u${seq}` },
  };
}

describe("GroupChatPanel answer quote", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  it("renders a chat-surface answer as an event line, not a bubble", () => {
    tail = [
      plainUserRow(1, "start the migration"),
      askCard(2, "card2", "Which schema?"),
      answerRow(3, "v2", "card2"),
    ];
    render(<GroupChatPanel />);

    // The receipt card is gone, and the answer is no longer an utterance
    // bubble either — a card interaction is a step of the run, so it states
    // itself as a row-tier event line naming the choice.
    expect(screen.queryByText("chatView.cardAsk")).toBeNull();
    // Nor may it fall through to the unanswered note: somebody did answer.
    // Without this the slot could paint "Unanswered: <question>" — a statement
    // the transcript would then have to contradict one line later.
    expect(screen.queryByText(/chatView.cardUnanswered/)).toBeNull();
    expect(screen.getByText("chatView.cardAnsweredNote«v2»")).toBeTruthy();
    expect(document.querySelector('[data-event-text="turn"]')).toBeTruthy();
    // The question was just read in the open card above — no quote strip.
    expect(screen.queryByText("Which schema?")).toBeNull();
  });

  it("paints no quote when in_reply_to points at a row that is not a card", () => {
    tail = [plainUserRow(1, "hello there"), answerRow(2, "reply to a plain message", "u1")];
    render(<GroupChatPanel />);
    // The referenced body appears exactly once — as its own bubble. A second
    // copy would mean it was also painted as a decision quote, and no decision
    // was ever asked here.
    expect(screen.getAllByText("hello there")).toHaveLength(1);
    expect(screen.getByText("reply to a plain message")).toBeTruthy();
  });

  it("keeps a quiet line for a card that closed with no answer", () => {
    tail = [plainUserRow(1, "start the migration"), askCard(2, "card2", "Which schema?")];
    // Close it without an answer: a terminal agent row carrying the same
    // trigger is what `closedCardIds` reads (§5.5.4).
    tail.push({
      seq: 3,
      ts: 3,
      from: AGENT,
      kind: "agent_message",
      body: "moving on",
      to: ME,
      meta: { message_id: "a3", in_reply_to: "u1", sender_session_id: "exec-9", status: "completed" },
    });
    render(<GroupChatPanel />);
    expect(screen.getByText("chatView.cardUnanswered«Which schema?»")).toBeTruthy();
    // Not a card shell any more: the live affordance is gone.
    expect(screen.queryByText("chatView.cardAsk")).toBeNull();
  });
});
