/**
 * DOM-shape guard for the bubble meta-anchor rule (`bubbleMetaAnchor.ts`).
 *
 * jsdom performs no layout, so this cannot measure where the time text is
 * painted. What it CAN pin is the structural half of the rule — the column
 * that carries the human's meta block (answer quote / "view run" / hover
 * timestamp) must anchor to the bubble's OWN edge, and the peer's must anchor
 * to the row's start edge. That is precisely the class-string drift that caused
 * the reported "sometimes in the middle, sometimes on the left":
 *
 *   AgentChatView's human column   `items-start` ✗ → timestamp under the
 *                                  bubble's LEFT edge (mid-row when the bubble
 *                                  shrink-wrapped, far left when it clamped)
 *   GroupChatPanel's human column  `items-end`   ✓ (the surface that made the
 *                                  drift visible by doing it right)
 *
 * Same harness shape as `chatBubbleWidth.test.tsx`: one store mock serving both
 * chat surfaces, rows injected through the window's tail.
 */
import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AgentChatView } from "./AgentChatView";
import { GroupChatPanel } from "./GroupChatPanel";
import { bubbleMetaAnchor } from "./bubbleMetaAnchor";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT = "agent-1";
const ME = "user-me";
const CHAT = "chat-1";
const GROUP = "group-1";

let chatTail: ChatMessageRow[] = [];
let groupTail: ChatMessageRow[] = [];

vi.mock("@tauri-apps/api/core", () => ({ convertFileSrc: (x: string) => x }));

vi.mock("../../stores/chatSessionStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../stores/chatSessionStore")>();
  return {
    ...actual,
    useChatSessionStore: Object.assign(
      (selector: (state: unknown) => unknown) =>
        selector({
          windows: {
            [CHAT]: { older: [], tail: chatTail, tailLoaded: chatTail.length, total: chatTail.length, loadingOlder: false },
            [GROUP]: { older: [], tail: groupTail, tailLoaded: groupTail.length, total: groupTail.length, loadingOlder: false },
          },
          sending: {},
          draft: {},
          attachedContext: {},
        }),
      {
        getState: () => ({
          sendText: vi.fn().mockResolvedValue(undefined),
          sendGroupText: vi.fn().mockResolvedValue(true),
          loadOlder: vi.fn().mockResolvedValue(undefined),
          loadNewer: vi.fn().mockResolvedValue(undefined),
          clearLocate: vi.fn(),
          setMode: vi.fn(),
          saveDraft: vi.fn().mockResolvedValue(undefined),
          upload: vi.fn().mockResolvedValue({ id: "a1", filename: "f", mime: "image/png", size: 1 }),
        }),
      },
    ),
  };
});

vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) =>
      selector({ availableModels: [], providerAccounts: [], agentStates: {} }),
    {
      getState: () => ({
        openSession: vi.fn().mockResolvedValue(undefined),
        seedOptimisticTitle: vi.fn(),
        selectAgent: vi.fn(),
      }),
    },
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
    (selector: (state: unknown) => unknown) => selector({ accessToken: "tok", account: { user_id: ME } }),
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
    t: (key: string, vars?: Record<string, unknown>) =>
      vars && Object.keys(vars).length ? `${key}|${Object.values(vars).join("|")}` : key,
  }),
}));

vi.mock("./ChatPanel", () => ({
  ModelMenu: () => <div data-testid="model-menu" />,
  ReasoningEffortMenu: () => <div data-testid="effort-menu" />,
}));
vi.mock("../workspace/WorkspaceSelector", () => ({
  WorkspaceSelector: () => <div data-testid="workspace-selector" />,
}));
vi.mock("../common/AgentAvatar", () => ({ AgentAvatar: () => <div /> }));
vi.mock("../common/UserAvatar", () => ({ UserAvatar: () => <div /> }));
vi.mock("../common/Tooltip", () => ({
  Tooltip: ({ children }: { children?: React.ReactNode }) => <>{children}</>,
}));
vi.mock("./CodeBlock", () => ({ CodeBlock: () => <div data-testid="code-block" /> }));
vi.mock("../editor/markdownLinkResolver", () => ({
  handleChatMarkdownLinkClick: vi.fn(),
  markdownUrlTransform: (x: string) => x,
}));

function agentRow(seq: number, body: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: AGENT,
    kind: "agent_message",
    body,
    to: ME,
    meta: { message_id: `a${seq}`, in_reply_to: "u1", status: "completed" },
  };
}

function humanRow(seq: number, body: string): ChatMessageRow {
  return { seq, ts: seq, from: ME, kind: "user_message", body, to: AGENT, meta: { message_id: `u${seq}` } };
}

/** The bubble that paints a message of this side. */
function bubbleOf(text: string, side: "agent" | "self"): HTMLElement | null {
  const cls = side === "agent" ? ".bg-chat-bubble" : ".bg-chat-user";
  return screen.getByText(text).closest<HTMLElement>(cls);
}

/** The bubble's column: the nearest ancestor (including the bubble's own
 *  parent) that declares a cross-axis anchor. */
function anchorOf(bubble: HTMLElement): string {
  let el: HTMLElement | null = bubble.parentElement;
  while (el) {
    const hit = el.className
      .split(/\s+/)
      .find((c) => c === "items-start" || c === "items-end");
    if (hit) return hit;
    el = el.parentElement;
  }
  return "";
}

describe("bubble meta-anchor rule (DOM shape)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
    chatTail = [];
    groupTail = [];
  });

  it("1:1 chat: the human's meta block hugs the human's own edge", () => {
    // A one-word answer is the case that exposed the drift: the column
    // shrink-wraps to the bubble, so a row-start anchor put the timestamp under
    // the bubble's LEFT edge — i.e. mid-row.
    chatTail = [humanRow(1, "批准")];
    render(<AgentChatView agentId={AGENT} sessionId={CHAT} />);
    const own = bubbleOf("批准", "self");
    expect(own).not.toBeNull();
    expect(anchorOf(own!)).toBe(bubbleMetaAnchor("own"));
    expect(anchorOf(own!)).toBe("items-end");
  });

  it("1:1 chat: a long human message keeps the same edge", () => {
    // The other half of the reported symptom: with a clamped bubble the anchor
    // decided the SAME timestamp's x moved to the far left.
    const long = "x".repeat(400);
    chatTail = [humanRow(1, long)];
    render(<AgentChatView agentId={AGENT} sessionId={CHAT} />);
    const own = bubbleOf(long, "self");
    expect(anchorOf(own!)).toBe("items-end");
  });

  it("1:1 chat: the agent side stays on the row's start edge", () => {
    chatTail = [humanRow(1, "review the split"), agentRow(2, "结论先行。")];
    render(<AgentChatView agentId={AGENT} sessionId={CHAT} />);
    const peer = bubbleOf("结论先行。", "agent");
    expect(peer).not.toBeNull();
    expect(anchorOf(peer!)).toBe(bubbleMetaAnchor("peer"));
  });

  it("group chat: both sides keep their own edge", () => {
    groupTail = [humanRow(1, "@Agent compare these"), agentRow(2, "看完了。")];
    render(<GroupChatPanel />);
    const own = bubbleOf("@Agent compare these", "self");
    const peer = bubbleOf("看完了。", "agent");
    expect(anchorOf(own!)).toBe("items-end");
    expect(anchorOf(peer!)).toBe("items-start");
  });
});