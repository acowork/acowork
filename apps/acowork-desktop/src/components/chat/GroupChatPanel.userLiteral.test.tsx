/**
 * GroupChatPanel — regression tests for the user/agent RENDERER SPLIT.
 *
 * Why this test exists:
 *   The reported defect: the user's own message bubble in the group chat (and
 *   in the user↔user DM, which shares this surface) painted its text black
 *   instead of white, while the user↔agent thread stayed white. Root cause:
 *   this surface routed EVERY row — `user_message` included — through
 *   `ChatMarkdown`. The prose shell sets `color: var(--tw-prose-body)` (a
 *   page-level near-black) on its own wrapper div, which beats the white
 *   `text-chat-user-text` the accent-filled own-bubble would otherwise
 *   inherit. The other two chat surfaces (`AgentChatView`,
 *   `UserWithAttachmentsBubble`) render human input as literal text and never
 *   hit the leak.
 *
 *   The fix restores the house grammar — human input is literal text,
 *   markdown is agent speech — so the contract pinned here is DOM shape, not
 *   CSS (jsdom computes no styles):
 *     1. a user row with markdown syntax renders LITERALLY: no `.prose`
 *        wrapper, no parsed `<strong>`/`<table>`, the raw characters survive;
 *     2. an agent row still renders through the `.prose` shell (the
 *        markdown-table contract from GroupChatPanel.markdownTable.test.tsx
 *        must keep holding — the split must not regress into "no markdown
 *        anywhere");
 *     3. the split follows the SPEAKER (row.kind), not the side: another
 *        human's message is literal too, even though its light bubble hides
 *        the color bug.
 *
 * Mock harness mirrors `GroupChatPanel.markdownTable.test.tsx`.
 */
import { render } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT = "agent-1";
const ME = "user-me";
const PEER = "user-peer";
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
            { id: PEER, identity: "human" },
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

/** Markdown syntax in a human message: emphasis, a table, a fenced block.
 *  All of it must survive as literal characters. */
const MARKDOWNISH_BODY = [
  "**bold** not so much",
  "",
  "| A | B |",
  "| - | - |",
  "| 1 | 2 |",
].join("\n");

function userRow(seq: number, from: string, body: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from,
    kind: "user_message",
    body,
    to: "",
    meta: { message_id: `u${seq}` },
  };
}

function agentRow(seq: number, body: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: AGENT,
    kind: "agent_message",
    body,
    to: ME,
    meta: { message_id: `a${seq}`, status: "completed" },
  };
}

describe("GroupChatPanel user/agent renderer split", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  it("renders the own user bubble as literal text — no .prose shell, no parsed markdown", () => {
    // The regression itself: with the user row routed through ChatMarkdown,
    // the prose wrapper's page-level text color overrode the bubble's white
    // (jsdom can't see the color, so we pin the absence of the prose scope).
    tail = [userRow(1, ME, MARKDOWNISH_BODY)];
    const { container } = render(<GroupChatPanel />);

    const bubble = container.querySelector(".bg-chat-user");
    expect(bubble, "own user bubble must render").not.toBeNull();
    expect(bubble!.querySelector(".prose"), "user bubble must NOT carry the prose markdown shell").toBeNull();
    expect(bubble!.querySelector("strong")).toBeNull();
    expect(bubble!.querySelector("table")).toBeNull();
    // The literal characters survive — emphasis and pipes unparsed.
    expect(bubble!.textContent).toContain("**bold** not so much");
    expect(bubble!.textContent).toContain("| A | B |");
  });

  it("renders another human's message as literal text too — the split follows the speaker, not the side", () => {
    tail = [userRow(1, PEER, "**bold** not so much")];
    const { container } = render(<GroupChatPanel />);

    const bubble = container.querySelector(".bg-chat-bubble");
    expect(bubble, "peer user bubble must render").not.toBeNull();
    expect(bubble!.querySelector(".prose")).toBeNull();
    expect(bubble!.querySelector("strong")).toBeNull();
    expect(bubble!.textContent).toContain("**bold** not so much");
  });

  it("still renders agent speech through the shared markdown shell", () => {
    // The other half of the contract: the fix must not swing into "no
    // markdown anywhere" — the table scope pinned by
    // GroupChatPanel.markdownTable.test.tsx stays intact for agent rows.
    tail = [agentRow(1, "**bold** agent speech")];
    const { container } = render(<GroupChatPanel />);

    const bubble = container.querySelector(".bg-chat-bubble");
    expect(bubble, "agent bubble must render").not.toBeNull();
    expect(bubble!.querySelector(".prose")).not.toBeNull();
    expect(bubble!.querySelector("strong")?.textContent).toBe("bold");
  });
});
