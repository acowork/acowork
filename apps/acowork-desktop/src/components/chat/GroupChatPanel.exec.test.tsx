/**
 * Group-chat "查看执行" affordance.
 *
 * ADR-106 D3/D8 moved the drill: it is no longer a chip on an agent's reply
 * bubble, it rides ABOVE the session's block. A block is one execution SESSION
 * (D4), so these tests feed reference rows plus their projection reads and assert
 * that the button routes through the exact calls AgentChatView.jumpToExec makes
 * — existence probe (a definitive 404 is a toast, no navigation) → selectAgent
 * (switch the middle pane to the agent) → openSession (cold-load the exec tab) →
 * setMode('exec') (flip the surface).
 *
 * What this pins that the block's own suite cannot: the group surface hands the
 * block the CHAT it must be participation-checked against, and drills into the
 * INSTANCE the projection names — not the row's `from`, which on a multi-party
 * surface is only who spoke.
 */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";
import type { ChatBlockEntry } from "../../lib/chatBlocks";

const AGENT = "agent-1";
const ME = "user-me";
const GROUP = "group-1";

// Jump collaborators — asserted per test; wired into the store mocks below.
let selectAgentMock: ReturnType<typeof vi.fn>;
let openSessionMock: ReturnType<typeof vi.fn>;
let setModeMock: ReturnType<typeof vi.fn>;

// Rows fed to the mocked message window; each test overwrites this.
let tail: ChatMessageRow[];
// The block read behind each round row; `null` = the read has not landed.
let blockEntry: ChatBlockEntry | null;

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
          setMode: (...a: unknown[]) => setModeMock(...(a as [])),
          sendGroupText: vi.fn().mockResolvedValue(true),
          loadOlder: vi.fn().mockResolvedValue(undefined),
          upload: vi.fn().mockResolvedValue({ id: "a1", filename: "f", mime: "image/png", size: 1 }),
        }),
      },
    ),
  };
});

// The block read is one entry per ref; batching is chatBlockStore's own suite.
// A block asks for its whole ref list (it is a SESSION's worth of rounds), so the
// mock answers per ref out of that list.
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
    {
      getState: () => ({
        openSession: (...a: unknown[]) => openSessionMock(...(a as [])),
        resolveQuestion: vi.fn(),
      }),
    },
  ),
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: Object.assign(
    (selector: (state: unknown) => unknown) =>
      selector({ agents: { [AGENT]: { meta: { name: "Agent A" } } } }),
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

/** One round's pointer row (D3): bodyless, and it names a run. */
function roundRow(
  seq: number,
  over: { instance?: string; execSid?: string; round?: string } = {},
): ChatMessageRow {
  const instance = over.instance ?? AGENT;
  const execSid = over.execSid ?? "exec-7";
  const round = over.round ?? `r${seq}`;
  return {
    seq,
    ts: seq,
    from: instance,
    kind: "execution_round",
    body: "",
    to: ME,
    meta: {
      message_id: `round:${execSid}|${round}`,
      agent_instance_id: instance,
      agent_session_id: execSid,
      round_id: round,
      in_reply_to: "u1",
    },
  };
}

/** What the block read answers for that round. */
function entryOf(instance: string, execSid: string, round: string): ChatBlockEntry {
  return {
    ref: `${instance}|${execSid}|${round}`,
    agent_instance_id: instance,
    agent_session_id: execSid,
    round_id: round,
    status: "ok",
    projection: {
      round_id: round,
      terminal: true,
      status: "completed",
      answer: { body: "pong", ts: "1" },
    },
  };
}

function userRow(seq: number, body: string): ChatMessageRow {
  return { seq, ts: seq, from: ME, kind: "user_message", body, to: AGENT, meta: { message_id: `u${seq}` } };
}

/**
 * The drill is icon-only since design 27 §5.5.5: it lives in the block's action
 * row above the block and its label is `aria-label`/`title`, not rendered text.
 * Every assertion below therefore queries by accessible name — a `getByText`
 * would pass on a regression that silently stopped drawing the control.
 */
/** Open the run the way a reader does: the block, then its newest round. Both
 *  start folded (`ChatExecutionBlock`'s contract) so a case about what is INSIDE
 *  has to ask for it. */
function openBlockAndNewestRound() {
  fireEvent.click(document.querySelector("[data-run-header]")!);
  fireEvent.click(screen.getByLabelText("chatView.block.toggleRound"));
}

describe("GroupChatPanel 查看执行 affordance", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    selectAgentMock = vi.fn();
    openSessionMock = vi.fn().mockResolvedValue(undefined);
    setModeMock = vi.fn();
    blockEntry = entryOf(AGENT, "exec-7", "r2");
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
  });

  it("jumps to the running agent's exec session via selectAgent → openSession → setMode('exec')", async () => {
    tail = [userRow(1, "ping"), roundRow(2)];
    render(<GroupChatPanel />);

    const btn = screen.getByRole("button", { name: "chatView.viewRun" });
    fireEvent.click(btn);

    // The jump now runs behind the existence probe (a rejected fetch is
    // inconclusive → proceed), so the whole chain lands a tick later.
    await waitFor(() => expect(selectAgentMock).toHaveBeenCalledWith(AGENT));
    expect(openSessionMock).toHaveBeenCalledWith(AGENT, "exec-7", { prepend: true });
    // setMode runs after the awaited openSession (a microtask later).
    await waitFor(() => expect(setModeMock).toHaveBeenCalledWith(AGENT, "exec"));
  });

  it("drills into the instance the projection names, not the row's `from`", async () => {
    // A multi-party surface can show a round whose host is not the speaker the
    // roster resolves — the block read is authoritative about where it ran.
    tail = [roundRow(2, { instance: "remote-agent", execSid: "exec-9", round: "r9" })];
    blockEntry = entryOf("remote-agent", "exec-9", "r9");
    render(<GroupChatPanel />);

    fireEvent.click(screen.getByRole("button", { name: "chatView.viewRun" }));
    await waitFor(() => expect(openSessionMock).toHaveBeenCalledWith("remote-agent", "exec-9", { prepend: true }));
  });

  it("the drill rides the block even when the round has said nothing yet", () => {
    tail = [roundRow(1, { execSid: "exec-9", round: "r1" })];
    blockEntry = {
      ...entryOf(AGENT, "exec-9", "r1"),
      projection: { round_id: "r1", terminal: true, status: "aborted", answer: null },
    };
    render(<GroupChatPanel />);
    expect(screen.queryByRole("button", { name: "chatView.viewRun" })).toBeTruthy();
    // The outcome lives on the round, and the round is folded by default
    // (`ChatExecutionBlock`'s own contract), so the reader opens both.
    openBlockAndNewestRound();
    // An outcome is reported whether or not there is a body to show.
    expect(screen.getByText("chatView.notice.interrupted")).toBeTruthy();
  });

  it("shows no drill on the human's own row", () => {
    tail = [userRow(1, "mine")];
    render(<GroupChatPanel />);
    expect(screen.queryByRole("button", { name: "chatView.viewRun" })).toBeNull();
  });

  it("shows no drill when the row names no resolvable round (D3)", () => {
    tail = [{ ...roundRow(2), meta: { ...roundRow(2).meta, agent_session_id: undefined } }];
    blockEntry = null;
    render(<GroupChatPanel />);
    expect(screen.queryByRole("button", { name: "chatView.viewRun" })).toBeNull();
    // The pointer still states something rather than vanishing into blank space.
    expect(screen.getByText("chatView.block.notFound")).toBeTruthy();
  });

  it("offers the retry AND the drill when the run cannot be read (D9)", () => {
    // The unreadable thing is the ROUND projection; the drill addresses the
    // execution SESSION, which is a different object and is not thereby gone —
    // the click probes it and toasts if it turns out to be deleted.
    tail = [roundRow(2)];
    blockEntry = { ...entryOf(AGENT, "exec-7", "r2"), status: "unreachable", projection: undefined, detail: "refused" };
    render(<GroupChatPanel />);
    expect(screen.queryByRole("button", { name: "chatView.viewRun" })).toBeTruthy();
    // The retry is inside the fold: the block starts collapsed.
    openBlockAndNewestRound();
    expect(screen.getByText("chatView.block.unreachable")).toBeTruthy();
    expect(screen.getByText("chatView.block.retry")).toBeTruthy();
  });

  it("the rounds of one session are one block, so the drill appears once per run", () => {
    // The failure this pins is the visual one: two reference rows of the same
    // execution session used to draw two cards, each with its own drill, and a
    // reader could not tell whether they were looking at one run or two.
    tail = [
      userRow(1, "ping"),
      roundRow(2, { round: "r2" }),
      roundRow(3, { round: "r3" }),
    ];
    blockEntry = entryOf(AGENT, "exec-7", "r3");
    render(<GroupChatPanel />);
    expect(screen.getAllByRole("button", { name: "chatView.viewRun" })).toHaveLength(1);
    fireEvent.click(screen.getByRole("button", { name: "chatView.viewRun" }));
    // One block, one session address — the newest round's, which is the same
    // session for every row in it.
    return waitFor(() =>
      expect(openSessionMock).toHaveBeenCalledWith(AGENT, "exec-7", { prepend: true }),
    );
  });

  it("toasts instead of jumping when the exec session has been deleted", async () => {
    // A definitive 404 from the existence probe (session meta AND live state
    // gone) must stop the drill: no selectAgent, no tab, no mode flip — just
    // a toast. The block's exec pointer outlives the session it names.
    tail = [roundRow(2)];
    (global.fetch as ReturnType<typeof vi.fn>).mockResolvedValue({ status: 404 });
    const toasts: Event[] = [];
    const listener = (e: Event) => toasts.push(e);
    window.addEventListener("acowork:toast", listener);
    try {
      render(<GroupChatPanel />);
      fireEvent.click(screen.getByRole("button", { name: "chatView.viewRun" }));
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
