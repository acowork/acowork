/**
 * Chat bubbles shrink-wrap (the agent bubble is `inline-block`, the group
 * bubble is a flex item of an `items-start` column), and shrink-to-fit falls
 * back to the content's MINIMUM width whenever the content cannot be squeezed:
 * a mermaid SVG's natural width, an unwrapped GFM table, one long unbreakable
 * token. Uncapped, the bubble then grew past the row's
 * `max-w-[var(--content-max-width)]` clamp — which sits on the PARENT — and the
 * reply spilled out of the chat column, clipped by the panel
 * (reported defect: an Architect answer with a wide diagram rendered past the
 * right edge, text cut mid-character).
 *
 * The contract that fixes it is on the bubble itself: `max-w-full` bounds the
 * used width to the row, and the overflow is handed to the scroll containers
 * that live inside (`CodeBlock`, `MermaidBlock`, `.prose-table-scroll`) — the
 * same contract Mobile's `.bubble { max-width: 100% }` has always had.
 *
 * jsdom performs no layout, so this guards the DOM shape rather than painted
 * geometry (same strategy as `markdownTable.test.tsx`): every shrink-wrap
 * bubble carries the clamp, and a chat markdown table is wrapped in the
 * scroller instead of being allowed to widen its bubble.
 */
import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AgentChatView } from "./AgentChatView";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT = "agent-1";
const ME = "user-me";
const CHAT = "chat-1";
const GROUP = "group-1";

let chatTail: ChatMessageRow[] = [];
let groupTail: ChatMessageRow[] = [];

vi.mock("@tauri-apps/api/core", () => ({ convertFileSrc: (x: string) => x }));

/** One store mock serving both surfaces: `windows` keyed per surface so each
 *  component only ever reads its own session. */
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
// Real CodeBlock shape is irrelevant here (it already owns its scroll); the
// mermaid path is stubbed so no diagram engine runs in jsdom.
vi.mock("./CodeBlock", () => ({ CodeBlock: () => <div data-testid="code-block" /> }));
vi.mock("../editor/markdownLinkResolver", () => ({
  handleChatMarkdownLinkClick: vi.fn(),
  markdownUrlTransform: (x: string) => x,
}));

const TABLE_BODY = `| file | verdict |
| --- | --- |
| core/acowork-runtime/src/agent/session/session_task.rs | keep |
| core/acowork-runtime/src/agent/loop_.rs | split |
`;

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

describe("chat bubble width contract", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
    chatTail = [];
    groupTail = [];
  });

  it("clamps the 1:1 agent bubble to its row", () => {
    chatTail = [humanRow(1, "review the runtime split"), agentRow(2, "结论先行：不建议拆分。")];
    render(<AgentChatView agentId={AGENT} sessionId={CHAT} />);
    const bubble = bubbleOf("结论先行：不建议拆分。", "agent");
    expect(bubble).not.toBeNull();
    // Shrink-wrap AND clamped — the pair is the whole contract.
    expect(bubble!.classList.contains("inline-block")).toBe(true);
    expect(bubble!.classList.contains("max-w-full")).toBe(true);
  });

  it("clamps the 1:1 human bubble the same way", () => {
    // A pasted path or URL is one unbreakable token; the bubble must scroll or
    // break it, never widen the row.
    chatTail = [humanRow(1, "https://example.com/a/very/long/path/that/never/ends")];
    render(<AgentChatView agentId={AGENT} sessionId={CHAT} />);
    const bubble = bubbleOf("https://example.com/a/very/long/path/that/never/ends", "self");
    expect(bubble?.classList.contains("max-w-full")).toBe(true);
  });

  it("scrolls a 1:1 markdown table instead of widening the bubble", () => {
    chatTail = [humanRow(1, "compare"), agentRow(2, TABLE_BODY)];
    const { container } = render(<AgentChatView agentId={AGENT} sessionId={CHAT} />);
    const wrappers = container.querySelectorAll(".prose-table-scroll");
    expect(wrappers).toHaveLength(1);
    expect(wrappers[0]!.querySelector("table")).not.toBeNull();
  });

  it("clamps the group bubble for both sides and scrolls its table", () => {
    groupTail = [humanRow(1, "@Agent compare these"), agentRow(2, TABLE_BODY)];
    const { container } = render(<GroupChatPanel />);
    // The agent row's body text lives inside the peer bubble.
    const peer = container.querySelector<HTMLElement>(".bg-chat-bubble");
    expect(peer?.classList.contains("max-w-full")).toBe(true);
    const self = container.querySelector<HTMLElement>(".bg-chat-user");
    expect(self?.classList.contains("max-w-full")).toBe(true);
    const wrappers = container.querySelectorAll(".prose-table-scroll");
    expect(wrappers).toHaveLength(1);
    expect(screen.getByText("@Agent compare these")).toBeTruthy();
  });
});
