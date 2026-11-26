// @vitest-environment jsdom
/**
 * GroupChatPanel — where a history-dialog jump actually lands.
 *
 * The reported defect: "群聊的右上角聊天记录 dialog 翻几页之后点击记录，定位不到正确的
 * 消息位置" — the pane moved, but onto a message that was not the one clicked.
 *
 * The layout folds a whole execution session's rounds into ONE virtual item
 * anchored at its oldest row (ADR-106 D3/D4), so the round a hit names is not an
 * item of its own: `keyOfSeq` resolves its seq to the BLOCK, and a locate that
 * asked only for that key centred the block. On a session that has been running
 * for a while the block is many screens tall, so the reader landed on whichever
 * round sat at its middle. Near the tail (a fresh page of hits) the two
 * positions are pixels apart and nobody noticed; deep in a paged-back history
 * they are different messages — which is why the report says "after paging".
 *
 * So the wiring this file pins is the anchor, not the pixels: a locate must ask
 * the list for the block key AND the row inside it (`[data-seq="…"]`), and
 * `VirtualRowList` itself is covered by `VirtualRowList.test.tsx`.
 */
import { act, render } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { GroupChatPanel } from "./GroupChatPanel";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT = "agent-1";
const ME = "user-me";
const GROUP = "group-1";
/** The exec session every round of the run belongs to — one block. */
const EXEC_SESSION = "exec-1";
/** The round the reader clicked in the dialog. */
const LOCATED_SEQ = 12;

const scrollToKey = vi.hoisted(() => vi.fn(() => true));
const clearLocate = vi.hoisted(() => vi.fn());

let winObj: Record<string, unknown> = {};

/**
 * The shell, reduced to its handle: the panel owns the scroll decisions, the
 * shell owns the pixels (pinned by its own suite). Renders nothing — the locate
 * hook's DOM fallback is deliberately not reached, because the handle answers.
 */
vi.mock("../common/VirtualRowList", () => {
  const handle = {
    getFirstVisibleIndex: () => 0,
    getFirstVisibleKey: () => null,
    getLastVisibleIndex: () => 0,
    isKeyInViewport: () => false,
    scrollToBottom: () => true,
    scrollToTop: () => {},
    scrollToIndex: () => {},
    scrollToKey,
  };
  return {
    VirtualRowList: (props: { ref?: React.MutableRefObject<unknown> }) => {
      if (props.ref) props.ref.current = handle;
      return null;
    },
  };
});

vi.mock("@tauri-apps/api/core", () => ({ convertFileSrc: (x: string) => x }));

vi.mock("../../stores/chatSessionStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../stores/chatSessionStore")>();
  return {
    ...actual,
    useChatSessionStore: Object.assign(
      (selector: (state: unknown) => unknown) =>
        selector({ windows: { [GROUP]: winObj }, sending: { [GROUP]: false }, inflight: {} }),
      { getState: () => ({ setMode: vi.fn(), clearLocate, loadOlder: vi.fn(), loadNewer: vi.fn() }) },
    ),
  };
});

vi.mock("../../stores/chatStore", () => {
  const state = { openSession: vi.fn(), seedOptimisticTitle: vi.fn(), resolveQuestion: vi.fn() };
  return {
    useChatStore: Object.assign(
      (selector: (state: unknown) => unknown) => selector({ agentStates: {} }),
      { getState: () => state },
    ),
  };
});

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
          title: "Group",
          participants: [
            { id: ME, identity: "human" },
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

vi.mock("../../lib/auth-api", () => ({ fetchDirectory: vi.fn().mockResolvedValue([]) }));
vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://127.0.0.1:1" }));
vi.mock("../../lib/stickToBottom", () => ({
  useStickToBottom: () => ({
    ref: { current: null },
    stickNow: () => {},
    unpin: () => {},
    isPinned: () => false,
    atBottom: true,
  }),
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
  markdownUrlTransform: (url: string) => url,
}));

/** One round of the run: bodyless reference row, as storage writes it. */
function roundRow(seq: number): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: AGENT,
    kind: "execution_round",
    body: "",
    to: ME,
    meta: {
      message_id: `r${seq}`,
      agent_instance_id: AGENT,
      agent_session_id: EXEC_SESSION,
      round_id: `round-${seq}`,
      round_name: "explore",
    },
  };
}

describe("GroupChatPanel history-jump anchor", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("no network in test")));
    // A paged-back window: the human's question, then six rounds of ONE exec
    // session (one block anchored at seq 1), then a round of a later session.
    const tail: ChatMessageRow[] = [
      {
        seq: 1,
        ts: 1,
        from: ME,
        kind: "user_message",
        body: "go",
        to: AGENT,
        meta: { message_id: "u1" },
      },
      ...Array.from({ length: 6 }, (_, i) => roundRow(10 + i)),
      {
        ...roundRow(30),
        meta: {
          message_id: "r30",
          agent_instance_id: AGENT,
          agent_session_id: "exec-2",
          round_id: "round-30",
        },
      },
    ];
    winObj = {
      older: [],
      tail,
      tailLoaded: tail.length,
      total: tail.length,
      firstPos: 0,
      lastPos: tail.length - 1,
      loadingOlder: false,
      loadingNewer: false,
      pendingLocateSeq: LOCATED_SEQ,
    };
  });

  it("asks for the block AND the row inside it, not the block alone", async () => {
    render(<GroupChatPanel />);
    // The locate runs in a passive effect on the commit that stamped it.
    await act(async () => {});

    expect(scrollToKey).toHaveBeenCalled();
    const [key, align, within] = scrollToKey.mock.calls[0] as unknown as [
      string,
      string,
      string | undefined,
    ];
    // The block that holds the round (one item for the whole exec session) …
    expect(key).toContain(`:run:${AGENT}|${EXEC_SESSION}`);
    // … and the ROW the reader clicked inside it. Without `within` the shell
    // centres the block, which on a long run is a different message.
    expect(align).toBe("center");
    expect(within).toBe(`[data-seq="${LOCATED_SEQ}"]`);
  });
});
