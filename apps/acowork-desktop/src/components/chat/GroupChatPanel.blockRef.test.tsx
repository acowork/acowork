/**
 * Group surface: the execution block's `@` action must be the same gesture as
 * typing `@` in the composer — it writes the agent's mention token into the
 * draft AND attaches that run as the agent's second-level reference.
 *
 * A `@` that only wrote the address record is invisible and inert: `addressRows`
 * draws only the picks the draft mentions, and `buildChatAddress` sends only
 * those, so the click used to leave the composer untouched and the reference
 * silently out of the message.
 */
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";
import type { ChatBlockEntry } from "../../lib/chatBlocks";

const AGENT = "agent-1";
const ME = "user-me";
const GROUP = "group-1";
const AGENT_NAME = "Agent A";
const EXEC = "exec-7";

let tail: ChatMessageRow[];
let blockEntry: ChatBlockEntry | null;
let sendGroupTextMock: ReturnType<typeof vi.fn>;

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
          sending: { [GROUP]: false },
          inflight: {},
        }),
      {
        getState: () => ({
          setMode: vi.fn(),
          sendGroupText: (...a: unknown[]) => sendGroupTextMock(...a),
          loadOlder: vi.fn().mockResolvedValue(undefined),
          upload: vi.fn().mockResolvedValue({ id: "a1", filename: "f", mime: "image/png", size: 1 }),
        }),
      },
    ),
  };
});

vi.mock("../../stores/chatBlockStore", () => ({
  useChatBlocks: (_chatId: string, refs: readonly string[]) => {
    const out: Record<string, ChatBlockEntry> = {};
    for (const ref of refs) if (blockEntry?.ref === ref) out[ref] = blockEntry;
    return out;
  },
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
      selector({ agents: { [AGENT]: { meta: { display_name: AGENT_NAME, name: "agent-a" } } } }),
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

function roundRow(seq: number, instance: string = AGENT): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: instance,
    kind: "execution_round",
    body: "",
    to: ME,
    meta: {
      message_id: `round:${EXEC}|r${seq}`,
      agent_instance_id: instance,
      agent_session_id: EXEC,
      round_id: `r${seq}`,
      in_reply_to: "u1",
    },
  };
}

function entryOf(instance: string = AGENT): ChatBlockEntry {
  return {
    ref: `${instance}|${EXEC}|r2`,
    agent_instance_id: instance,
    agent_session_id: EXEC,
    round_id: "r2",
    status: "ok",
    projection: {
      round_id: "r2",
      terminal: true,
      status: "completed",
      answer: { body: "pong", ts: "1" },
    },
  };
}

function userRow(seq: number, body: string): ChatMessageRow {
  return { seq, ts: seq, from: ME, kind: "user_message", body, to: AGENT, meta: { message_id: `u${seq}` } };
}

/** The panel's own composer — a block's action row draws a second textarea (its
 *  in-place reply drawer), so `document.querySelector("textarea")` is not it. */
function composer(): HTMLTextAreaElement {
  return document.querySelector<HTMLTextAreaElement>('textarea[aria-label="groupChat.send"]')!;
}

/** Click the block's `@`, then let the caret/focus animation frame the insert
 *  schedules land inside `act` — it re-syncs the mention picker's state. */
async function clickBlockReference(): Promise<void> {
  fireEvent.click(screen.getByRole("button", { name: "chatView.block.actionReference" }));
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

describe("GroupChatPanel block @ action", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    blockEntry = entryOf();
    tail = [userRow(1, "ping"), roundRow(2)];
    sendGroupTextMock = vi.fn().mockResolvedValue(true);
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  it("inserts the agent's @ mention and attaches the run, like the composer's @", async () => {
    render(<GroupChatPanel />);
    await clickBlockReference();
    // The mention token is the visible half of the gesture: both the chip row
    // and the send key off `resolveMentions(draft)`.
    expect(composer().value).toBe(`@${AGENT_NAME} `);
    // The chip proves the address record took the run as its second level.
    expect(
      screen.getByRole("button", { name: "chatView.address.clear" }),
    ).toBeTruthy();
  });

  it("keeps what the human already typed and lands the mention at the caret", async () => {
    render(<GroupChatPanel />);
    fireEvent.change(composer(), { target: { value: "hello " } });
    composer().setSelectionRange(6, 6);
    await clickBlockReference();
    expect(composer().value).toBe(`hello @${AGENT_NAME} `);
  });

  it("addresses the next send to that agent AND that run", async () => {
    render(<GroupChatPanel />);
    await clickBlockReference();
    fireEvent.change(composer(), {
      target: { value: `@${AGENT_NAME} 看看这个` },
    });
    fireEvent.keyDown(composer(), { key: "Enter" });
    await waitFor(() => expect(sendGroupTextMock).toHaveBeenCalledTimes(1));
    const [chatId, body, targets, , , address] = sendGroupTextMock.mock.calls[0]!;
    expect(chatId).toBe(GROUP);
    expect(body).toBe(`@${AGENT_NAME} 看看这个`);
    expect(targets).toEqual([AGENT]);
    expect(address).toEqual({
      refs: [{ agentInstanceId: AGENT, sessionId: EXEC }],
    });
  });

  it("attaches a second run without naming the agent twice", async () => {
    render(<GroupChatPanel />);
    // The agent is already in the draft (picked by hand, or attached from
    // another block): a merge, not a second `@`.
    fireEvent.change(composer(), { target: { value: `@${AGENT_NAME} ` } });
    await clickBlockReference();
    expect(composer().value).toBe(`@${AGENT_NAME} `);
    expect(
      screen.getByRole("button", { name: "chatView.address.clear" }),
    ).toBeTruthy();
  });

  it("refuses an agent this chat cannot name instead of writing a dead token", async () => {
    // A run whose host is not a member of this chat: `resolveMentions` could
    // never resolve a token for it, so the click says so and writes nothing.
    const stranger = "remote-agent";
    tail = [userRow(1, "ping"), roundRow(2, stranger)];
    blockEntry = entryOf(stranger);
    const toasts: Event[] = [];
    const listener = (e: Event) => toasts.push(e);
    window.addEventListener("acowork:toast", listener);
    try {
      render(<GroupChatPanel />);
      await clickBlockReference();
      expect(composer().value).toBe("");
      expect(
        (toasts[0] as CustomEvent<{ message: string }>).detail.message,
      ).toBe("chatView.address.notAddressable");
    } finally {
      window.removeEventListener("acowork:toast", listener);
    }
  });

  it("does not double the token or the chip when the block is clicked twice", async () => {
    render(<GroupChatPanel />);
    await clickBlockReference();
    await clickBlockReference();
    expect(composer().value).toBe(`@${AGENT_NAME} `);
    expect(
      screen.getAllByRole("button", { name: "chatView.address.clear" }),
    ).toHaveLength(1);
  });
});
