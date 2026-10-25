/**
 * ADR-100's derived waiting line asserts "an agent is working on the newest
 * message". It must therefore appear ONLY for a chat where a human row actually
 * woke an agent — a `user_user` DM has no agent at all, and the reported defect
 * was it sitting there shimmering "正在处理…" on open, forever, because the
 * newest `user_message` had no answering `agent_message` and never could have.
 *
 * The rule is a row fact, not a guess (see `chatTurnState.opensAgentTurn`):
 * `to` names the delivered agent on the chat-message channel, `meta.mentions`
 * names them on the §5.3.1 fan-out row, and the archive-only write entry stores
 * an empty `to` — which is what a DM and an un-@'d group remark are.
 *
 * Mock harness mirrors `GroupChatPanel.execLabels.test.tsx`.
 */
import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT = "agent-1";
const ME = "user-me";
const PEER = "user-peer";
const GROUP = "group-1";

let tail: ChatMessageRow[];
let sendInFlight = false;
/** Swapped per case: a project group (has an agent) or a `user_user` DM (two people). */
let session: { session_id: string; kind: string; title: string; participants: Array<{ id: string; identity: string }> };

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
          sending: { [GROUP]: sendInFlight },
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
    selector({ activeGroupId: GROUP, groups: [session] }),
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

const PENDING = "chatView.notice.pendingTurnGeneric";

/** A human row as each writer files it: `to` set by chat-message, `mentions` by
 *  the fan-out, neither by the archive-only entry (DMs and plain group remarks). */
function human(seq: number, body: string, opt: { to?: string; mentions?: string[] } = {}): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: ME,
    kind: "user_message",
    body,
    to: opt.to ?? "",
    meta: { message_id: `u${seq}`, ...(opt.mentions ? { mentions: opt.mentions } : {}) },
  };
}

function agentReply(seq: number, inReplyTo: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: AGENT,
    kind: "agent_message",
    body: "done",
    to: ME,
    meta: { message_id: `a:${inReplyTo}`, in_reply_to: inReplyTo, status: "completed" },
  };
}

const asDm = () => ({
  session_id: GROUP,
  kind: "user_user",
  title: "",
  participants: [
    { id: ME, identity: "human" },
    { id: PEER, identity: "human" },
  ],
});

const asGroup = () => ({
  session_id: GROUP,
  kind: "project_group",
  title: "Group",
  participants: [
    { id: ME, identity: "human" },
    { id: AGENT, identity: "agent" },
  ],
});

describe("GroupChatPanel derived waiting line", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
    sendInFlight = false;
    session = asGroup();
  });

  it("shows nothing in a user_user DM whose last row is unanswered", () => {
    // The reported bug: with two people and no agent, the newest `user_message`
    // can never gain an answering `agent_message`, so the old rule held a
    // shimmer open from the moment the DM was opened.
    session = asDm();
    tail = [human(1, "are you coming?")];
    render(<GroupChatPanel />);
    expect(screen.queryByText(PENDING)).toBeNull();
  });

  it("shows nothing in a DM even while the send round trip is in flight", () => {
    session = asDm();
    sendInFlight = true;
    tail = [];
    render(<GroupChatPanel />);
    expect(screen.queryByText(PENDING)).toBeNull();
  });

  it("shows nothing for an un-@'d group remark — it woke nobody", () => {
    tail = [human(1, "noting this for the record")];
    render(<GroupChatPanel />);
    expect(screen.queryByText(PENDING)).toBeNull();
  });

  it("waits on an @-fan-out row: one broadcast row, mentioned agents in meta", () => {
    tail = [human(1, "@Architect review this", { mentions: [AGENT] })];
    render(<GroupChatPanel />);
    expect(screen.getByText(PENDING)).toBeTruthy();
  });

  it("waits on a row delivered to a named agent and stops at its terminal reply", () => {
    tail = [human(1, "review this", { to: AGENT })];
    const { unmount } = render(<GroupChatPanel />);
    expect(screen.getByText(PENDING)).toBeTruthy();
    unmount();

    tail = [human(1, "review this", { to: AGENT }), agentReply(2, "u1")];
    render(<GroupChatPanel />);
    expect(screen.queryByText(PENDING)).toBeNull();
  });

  it("keeps waiting through a plain remark that landed after the running turn", () => {
    // A row that woke nobody neither answers the turn nor supersedes it — the
    // agent is still working on the message that actually asked something.
    tail = [human(1, "@Architect review this", { mentions: [AGENT] }), human(2, "btw the repo moved")];
    render(<GroupChatPanel />);
    expect(screen.getByText(PENDING)).toBeTruthy();
  });
});
