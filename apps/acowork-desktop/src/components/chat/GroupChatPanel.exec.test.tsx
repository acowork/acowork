/**
 * Group-chat "查看执行" affordance.
 *
 * The group twin of the 1:1 chat surface's drill: an agent reply carries a
 * resolved exec session (`sender_session_id` / `agent_session_id`), and its
 * button must route through the exact calls AgentChatView.jumpToExec makes —
 * existence probe (a definitive 404 is a toast, no navigation) → selectAgent
 * (switch the middle pane to the agent) → openSession (cold-load the exec
 * tab) → setMode('exec') (flip the surface). Rows that
 * are the human's own, carry no exec sid, or name a non-local agent (so are
 * not addressable) must NOT show the button.
 */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT = "agent-1";
const ME = "user-me";
const GROUP = "group-1";

// Jump collaborators — asserted per test; wired into the store mocks below.
let selectAgentMock: ReturnType<typeof vi.fn>;
let openSessionMock: ReturnType<typeof vi.fn>;
let setModeMock: ReturnType<typeof vi.fn>;

// Rows fed to the mocked message window; each test overwrites this.
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
          live: { [GROUP]: { inFlight: null } },
          sending: { [GROUP]: false },
        }),
      {
        getState: () => ({
          setMode: (...a: unknown[]) => setModeMock(...(a as [])),
          sendGroupText: vi.fn().mockResolvedValue(true),
          loadOlder: vi.fn().mockResolvedValue(undefined),
          upload: vi.fn().mockResolvedValue({ id: "a1", filename: "f", mime: "image/png", size: 1 }),
        }),
      },
    ),
  };
});

vi.mock("../../stores/chatStore", () => ({
  useChatStore: {
    getState: () => ({ openSession: (...a: unknown[]) => openSessionMock(...(a as [])) }),
  },
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: Object.assign(
    (selector: (state: unknown) => unknown) =>
      selector({ agents: { [AGENT]: { meta: { display_name: "Agent A", name: "agent-a" } } } }),
    { getState: () => ({ selectAgent: (...a: unknown[]) => selectAgentMock(...(a as [])) }) },
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

function agentRow(seq: number, body: string, extra?: Record<string, unknown>): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: AGENT,
    kind: "agent_message",
    body,
    to: ME,
    meta: { message_id: `a${seq}`, ...(extra ?? {}) },
  };
}
function userRow(seq: number, body: string): ChatMessageRow {
  return { seq, ts: seq, from: ME, kind: "user_message", body, to: AGENT, meta: { message_id: `u${seq}` } };
}

describe("GroupChatPanel 查看执行 affordance", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    selectAgentMock = vi.fn();
    openSessionMock = vi.fn().mockResolvedValue(undefined);
    setModeMock = vi.fn();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  it("jumps to the answering agent's exec session via selectAgent → openSession → setMode('exec')", async () => {
    tail = [userRow(1, "ping"), agentRow(2, "pong", { sender_session_id: "exec-7" })];
    render(<GroupChatPanel />);

    const btn = screen.getByText("chatView.viewRun");
    fireEvent.click(btn);

    // The jump now runs behind the existence probe (a rejected fetch is
    // inconclusive → proceed), so the whole chain lands a tick later.
    await waitFor(() => expect(selectAgentMock).toHaveBeenCalledWith(AGENT));
    expect(openSessionMock).toHaveBeenCalledWith(AGENT, "exec-7");
    // setMode runs after the awaited openSession (a microtask later).
    await waitFor(() => expect(setModeMock).toHaveBeenCalledWith(AGENT, "exec"));
  });

  it("prefers agent_session_id when both pointers are present", async () => {
    tail = [agentRow(1, "pong", { agent_session_id: "exec-router", sender_session_id: "exec-sender" })];
    render(<GroupChatPanel />);

    fireEvent.click(screen.getByText("chatView.viewRun"));
    await waitFor(() => expect(openSessionMock).toHaveBeenCalledWith(AGENT, "exec-router"));
  });

  it("renders on an empty-body reply that still carries a session pointer", () => {
    tail = [agentRow(1, "", { sender_session_id: "exec-9", status: "completed" })];
    render(<GroupChatPanel />);
    expect(screen.queryByText("chatView.viewRun")).toBeTruthy();
  });

  it("omits the button on the human's own row", () => {
    tail = [userRow(1, "mine")];
    render(<GroupChatPanel />);
    expect(screen.queryByText("chatView.viewRun")).toBeNull();
  });

  it("omits the button on an agent reply with no resolved exec session", () => {
    tail = [agentRow(1, "no pointer")];
    render(<GroupChatPanel />);
    expect(screen.queryByText("chatView.viewRun")).toBeNull();
  });

  it("omits the button for a non-local (unaddressable) agent", () => {
    // `from` is not in the mocked agents registry → nameOf reports isAgent=false
    // → the drill target cannot be addressed, so no button.
    tail = [{ ...agentRow(1, "ghost", { sender_session_id: "exec-1" }), from: "remote-agent" }];
    render(<GroupChatPanel />);
    expect(screen.queryByText("chatView.viewRun")).toBeNull();
  });

  it("toasts instead of jumping when the exec session has been deleted", async () => {
    // A definitive 404 from the existence probe (session meta AND live state
    // gone) must stop the drill: no selectAgent, no tab, no mode flip — just
    // a toast. The message's exec pointer outlives the session it names.
    tail = [agentRow(1, "pong", { sender_session_id: "exec-gone" })];
    (global.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({ status: 404 });
    const toasts: Event[] = [];
    const listener = (e: Event) => toasts.push(e);
    window.addEventListener("acowork:toast", listener);
    try {
      render(<GroupChatPanel />);
      fireEvent.click(screen.getByText("chatView.viewRun"));
      await waitFor(() => expect(toasts.length).toBe(1));
      expect(selectAgentMock).not.toHaveBeenCalled();
      expect(openSessionMock).not.toHaveBeenCalled();
      expect(setModeMock).not.toHaveBeenCalled();
      const detail = (toasts[0] as CustomEvent<{ message: string }>).detail;
      expect(detail.message).toBe("chatView.execSessionGone");
    } finally {
      window.removeEventListener("acowork:toast", listener);
    }
  });
});
