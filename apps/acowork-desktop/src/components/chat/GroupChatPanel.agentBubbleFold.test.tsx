// @vitest-environment jsdom
/**
 * GroupChatPanel — the fold on an agent's bubble in a group chat.
 *
 * Why this test exists:
 *   A group's agent bubbles were unclamped, and an agent's answer is usually
 *   LONG — the PM's task message to the group is the common case. One such
 *   bubble is a wall of text, and a group transcript is mostly such bubbles,
 *   so the reader scrolls past walls instead of paging through a
 *   conversation. The fold shows the first ten lines and a chevron that opens
 *   the rest — the same grammar the run answer under an execution block uses.
 *
 * What is pinned here:
 *   1. an over-long agent bubble is FOLDED and offers the chevron;
 *   2. a short agent bubble is NOT folded — a fold control that does nothing
 *      is noise, and the fold is not allowed to appear on every row;
 *   3. the human's own bubble is NOT folded: human input is literal text,
 *      and a human's message is not the agent's wall of text;
 *   4. the chevron opens the rest.
 *
 * Mock harness mirrors `GroupChatPanel.markdownTable.test.tsx`.
 */
import { fireEvent, render } from "@testing-library/react";
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

/** An agent reply row carrying `body` verbatim — terminal status, non-empty,
 *  so it paints as an ordinary bubble (not the empty-turn status line). */
function agentReply(seq: number, body: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: AGENT,
    kind: "agent_message",
    body,
    to: ME,
    meta: { message_id: `a${seq}`, in_reply_to: `u${seq}`, status: "completed" },
  };
}

/** A human's own row. */
function humanSay(seq: number, body: string): ChatMessageRow {
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

/** A long message — the PM's task dispatch is the common case. */
function longBody(lines = 40): string {
  return Array.from({ length: lines }, (_, i) => `line ${i + 1}`).join("\n");
}

describe("GroupChatPanel agent-bubble fold", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  it("folds an over-long agent bubble and offers the chevron", () => {
    tail = [agentReply(1, longBody())];
    const { container } = render(<GroupChatPanel />);
    const btn = container.querySelector("[data-chat-fold-toggle]");
    expect(btn, "a long agent bubble must offer the fold").not.toBeNull();
    expect(btn!.getAttribute("aria-expanded")).toBe("false");
    // The fold is a real ceiling on the content.
    const clip = container.querySelector("[data-chat-fold-clip]") as HTMLElement;
    expect(clip.style.maxHeight).not.toBe("");
  });

  it("does not fold a short agent bubble", () => {
    tail = [agentReply(1, "on it")];
    const { container } = render(<GroupChatPanel />);
    // A fold control that does nothing is noise — the fold belongs to the
    // long bubbles only.
    expect(container.querySelector("[data-chat-fold-toggle]")).toBeNull();
    expect(container.textContent).toContain("on it");
  });

  it("does not fold the human's own bubble", () => {
    tail = [humanSay(1, longBody())];
    const { container } = render(<GroupChatPanel />);
    // Human input is literal text and is not the agent's wall of text; the
    // fold is the agent's speech, not the reader's own words.
    expect(container.querySelector("[data-chat-fold-toggle]")).toBeNull();
  });

  it("the chevron opens the rest of the answer", () => {
    tail = [agentReply(1, longBody())];
    const { container } = render(<GroupChatPanel />);
    const btn = container.querySelector("[data-chat-fold-toggle]") as HTMLElement;
    const clip = container.querySelector("[data-chat-fold-clip]") as HTMLElement;
    const folded = parseFloat(clip.style.maxHeight);
    fireEvent.click(btn);
    expect(
      container.querySelector("[data-chat-fold-toggle]")!.getAttribute("aria-expanded"),
    ).toBe("true");
    const expanded = parseFloat(
      (container.querySelector("[data-chat-fold-clip]") as HTMLElement).style.maxHeight,
    );
    expect(expanded).toBeGreaterThan(folded);
  });
});
