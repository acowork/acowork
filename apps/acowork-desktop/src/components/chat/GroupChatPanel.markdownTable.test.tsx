/**
 * GroupChatPanel — regression tests for markdown TABLES in agent replies.
 *
 * Why this test exists:
 *   The reported defect: an agent message containing a GFM markdown table
 *   rendered as unstyled text in the project group chat, while the very same
 *   reply rendered as a proper table in the execution session view. The
 *   parsing pipeline was never the problem — `ChatMarkdown` already runs
 *   remark-gfm and emits a real <table>. What the group surface lacked was
 *   the `.prose` shell every other chat surface wraps its ReactMarkdown in
 *   (`AgentChatView` / `MessageBubble`): every table rule in globals.css
 *   (`.prose table` frame, `.prose th` / `.prose td` borders + padding,
 *   header shade, zebra override) is scoped under `.prose`, so a table
 *   outside that scope parses but paints as plain text.
 *
 *   jsdom doesn't compute CSS, so we pin the DOM-shape contract instead:
 *     1. a <table> element exists in the rendered bubble (remark-gfm stays
 *        wired into the group pipeline),
 *     2. the <table> sits inside a `.prose` ancestor (the style scope —
 *        removing the shell wrapper from ChatMarkdown turns this red),
 *     3. the <table> sits inside `.prose-table-scroll` (the horizontal
 *        scroll wrapper shared by every desktop markdown surface).
 *
 * Mock harness mirrors `GroupChatPanel.waitLine.test.tsx`.
 */
import { render } from "@testing-library/react";
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

const TABLE_BODY = [
  "Here is the review summary:",
  "",
  "| File | Issue | Severity |",
  "| --- | --- | --- |",
  "| proxy.rs | Missing 5xx mapping | high |",
  "| runner.rs | Empty reply not degraded | medium |",
  "",
  "Nothing else to report.",
].join("\n");

describe("GroupChatPanel agent-reply markdown tables", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  it("renders a GFM table in an agent bubble as a real <table> with cells", () => {
    tail = [agentReply(1, TABLE_BODY)];
    const { container } = render(<GroupChatPanel />);

    const table = container.querySelector("table");
    expect(table, "agent bubble must contain a parsed <table>, not raw pipe text").not.toBeNull();
    // Header + body cells come from remark-gfm's parse, not from our
    // overrides — their presence pins remarkGfm in the group pipeline.
    expect(table!.querySelectorAll("th")).toHaveLength(3);
    expect(table!.querySelectorAll("tbody td")).toHaveLength(6);
    // The raw pipes must NOT leak into the rendered text.
    expect(container.textContent).not.toContain("| File | Issue | Severity |");
  });

  it("wraps the table in a .prose scope — the style contract every table rule in globals.css depends on", () => {
    // This is the regression itself: the group ChatMarkdown once rendered
    // ReactMarkdown without the `.prose` shell the execution-session
    // surfaces use, so `.prose table` / `.prose th` / `.prose td` never
    // matched and the table painted as plain text.
    tail = [agentReply(1, TABLE_BODY)];
    const { container } = render(<GroupChatPanel />);

    const table = container.querySelector("table");
    expect(table).not.toBeNull();
    const proseScope = table!.closest(".prose");
    expect(proseScope, "table must render inside a .prose ancestor (globals.css table styles are .prose-scoped)").not.toBeNull();
  });

  it("keeps the table inside the shared .prose-table-scroll wrapper", () => {
    // Same contract markdownTable.test.tsx pins for the other surfaces:
    // a wide table must scroll inside the bubble, not widen it.
    tail = [agentReply(1, TABLE_BODY)];
    const { container } = render(<GroupChatPanel />);

    const table = container.querySelector("table");
    expect(table).not.toBeNull();
    expect(table!.closest(".prose-table-scroll")).not.toBeNull();
  });

  it("surrounding paragraphs still render (table is a block inside the flow, not the whole body)", () => {
    tail = [agentReply(1, TABLE_BODY)];
    const { container } = render(<GroupChatPanel />);

    expect(container.textContent).toContain("Here is the review summary:");
    expect(container.textContent).toContain("Nothing else to report.");
    expect(container.querySelectorAll("table")).toHaveLength(1);
  });
});
