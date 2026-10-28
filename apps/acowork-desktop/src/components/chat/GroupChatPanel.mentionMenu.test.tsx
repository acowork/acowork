// @vitest-environment jsdom
/**
 * The group-chat "@agent" right-click menu: right-click an agent's avatar or
 * name → the menu offers one item → clicking it drops `@<name> ` into the
 * composer at the caret.
 *
 * The inserted text is the whole contract: `resolveMentions` matches
 * `@<display name>` to wake the agent, so the menu must insert exactly the
 * token the picker would have inserted.
 */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT = "agent-a";
const ME = "user-me";
const GROUP = "group-1";
// Hoisted: the `vi.mock` factories run before the module body, so the id
// must live in a hoisted binding.
const HUMAN = vi.hoisted(() => "user-other");

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
            [GROUP]: {
              older: [],
              tail,
              tailLoaded: tail.length,
              total: tail.length,
              loadingOlder: false,
            },
          },
          sending: {},
        }),
      {
        getState: () => ({
          setMode: vi.fn(),
          sendGroupText: vi.fn().mockResolvedValue(true),
          loadOlder: vi.fn().mockResolvedValue(undefined),
          loadNewer: vi.fn().mockResolvedValue(undefined),
          clearLocate: vi.fn(),
          upload: vi.fn(),
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
      selector({
        agents: {
          [AGENT]: { meta: { display_name: "Agent A", name: "agent-a" } },
        },
      }),
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
            { id: HUMAN, identity: "human" },
            { id: AGENT, identity: "agent" },
          ],
        },
      ],
      dms: [],
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
  // The human participant resolves through the account directory, same as prod.
  fetchDirectory: vi
    .fn()
    .mockResolvedValue([{ user_id: HUMAN, username: "other", display_name: "Other Human" }]),
}));
vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://127.0.0.1:1" }));
vi.mock("../../lib/stickToBottom", () => ({
  useStickToBottom: () => ({
    ref: { current: null },
    stickNow: vi.fn(),
    atBottom: true,
  }),
}));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, vars?: Record<string, unknown>) =>
      vars && "name" in vars ? `${key}:${vars.name}` : key,
  }),
}));

vi.mock("../common/AgentAvatar", () => ({
  AgentAvatar: () => <div data-testid="agent-avatar" />,
}));
vi.mock("../common/UserAvatar", () => ({
  UserAvatar: () => <div data-testid="user-avatar" />,
}));
vi.mock("../common/Tooltip", () => ({
  Tooltip: ({ children }: { children?: React.ReactNode }) => <>{children}</>,
}));

function humanRow(seq: number): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: HUMAN,
    kind: "user_message",
    body: "hi all",
    to: "",
    meta: { message_id: `h${seq}` },
  };
}

function agentRow(seq: number): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: AGENT,
    kind: "agent_message",
    body: "answer",
    to: ME,
    meta: { message_id: `a${seq}` },
  };
}

function composer(): HTMLTextAreaElement {
  return screen.getByPlaceholderText(
    "groupChat.placeholderWithMentions",
  ) as HTMLTextAreaElement;
}

describe("GroupChatPanel @-mention right-click menu", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
    tail = [agentRow(1)];
  });

  it("right-clicking an agent's name opens a menu that fills @name into the composer", async () => {
    render(<GroupChatPanel />);

    fireEvent.contextMenu(screen.getByText("Agent A"));

    const item = await screen.findByText("groupChat.mentionMenu:Agent A");
    fireEvent.click(item);

    await waitFor(() => expect(composer().value).toBe("@Agent A "));
  });

  it("right-clicking the avatar opens the same menu", async () => {
    render(<GroupChatPanel />);

    fireEvent.contextMenu(screen.getByTestId("agent-avatar"));

    fireEvent.click(await screen.findByText("groupChat.mentionMenu:Agent A"));

    await waitFor(() => expect(composer().value).toBe("@Agent A "));
  });

  it("right-clicking another human member offers the same @ menu", async () => {
    tail = [humanRow(1)];
    render(<GroupChatPanel />);

    // The human's display name resolves through the account directory,
    // which the panel fetches on mount — wait for it to land.
    fireEvent.contextMenu(await screen.findByText("Other Human"));

    fireEvent.click(await screen.findByText("groupChat.mentionMenu:Other Human"));

    await waitFor(() => expect(composer().value).toBe("@Other Human "));
  });

  it("inserts at the caret, keeping the text before it", async () => {
    render(<GroupChatPanel />);
    const ta = composer();
    fireEvent.change(ta, { target: { value: "hi " } });
    ta.setSelectionRange(3, 3);

    fireEvent.contextMenu(screen.getByText("Agent A"));
    fireEvent.click(await screen.findByText("groupChat.mentionMenu:Agent A"));

    await waitFor(() => expect(composer().value).toBe("hi @Agent A "));
  });
});
