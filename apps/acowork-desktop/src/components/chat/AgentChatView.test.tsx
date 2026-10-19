import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AgentChatView } from "./AgentChatView";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const AGENT = "agent-1";
const SESSION = "chat-1";

// ── chat-session store ────────────────────────────────────────────────
// `windows` / `inFlight` / `sending` / `draft` are read through the hook;
// the composer actions through `getState`. `answeredCardIds` is re-implemented
// verbatim (mirrors the store) so the view's answered detection is exercised.
let windows: Record<string, unknown>;
let inFlightMap: Record<string, unknown>;

function answeredCardIds(rows: ChatMessageRow[]): Set<string> {
  return new Set(
    rows.map((r) => r.meta?.in_reply_to).filter((s): s is string => !!s),
  );
}

vi.mock("../../stores/chatSessionStore", () => ({
  answeredCardIds,
  useChatSessionStore: Object.assign(
    (selector: (state: unknown) => unknown) =>
      selector({ windows, inFlight: inFlightMap, sending: {}, draft: {} }),
    {
      getState: () => ({
        sendText: vi.fn().mockResolvedValue(undefined),
        loadOlder: vi.fn().mockResolvedValue(undefined),
        setMode: vi.fn(),
        saveDraft: vi.fn().mockResolvedValue(undefined),
        upload: vi.fn().mockResolvedValue({ id: "a1", filename: "f", mime: "image/png", size: 1 }),
      }),
    },
  ),
}));

vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) =>
      selector({ availableModels: [], providerAccounts: [] }),
    { getState: () => ({ openSession: vi.fn().mockResolvedValue(undefined) }) },
  ),
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (state: unknown) => unknown) =>
    selector({ agents: { [AGENT]: { meta: { display_name: "Agent A", name: "agent-a" } } } }),
}));

vi.mock("../../stores/authStore", () => ({
  useAuthStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector({ accessToken: "tok" }),
    { getState: () => ({ accessToken: "tok" }) },
  ),
}));

vi.mock("../../stores/userProfileStore", () => ({
  useUserProfileStore: (selector: (state: unknown) => unknown) =>
    selector({
      profile: { displayName: "You", backendAvatarUrl: null, backendBuiltinAvatarId: null },
    }),
}));

vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://127.0.0.1:1" }));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

// The unified status card is the collaborator under test: stub it so its
// presence/absence and the props the view wires into it are observable,
// without pulling the whole live-mirror store graph.
vi.mock("./ExecSessionStatusCard", () => ({
  ExecSessionStatusCard: ({ card }: { card?: { kind: string; messageId: string } }) => (
    <div
      data-testid="status-card"
      data-kind={card?.kind ?? ""}
      data-msgid={card?.messageId ?? ""}
    />
  ),
}));

// Toolbar / editor deps that pull unrelated stores — neutral stubs.
vi.mock("./ChatPanel", () => ({
  ModelMenu: () => <div data-testid="model-menu" />,
  ReasoningEffortMenu: () => <div data-testid="effort-menu" />,
}));
vi.mock("../workspace/WorkspaceSelector", () => ({
  WorkspaceSelector: () => <div data-testid="workspace-selector" />,
}));
vi.mock("./CodeBlock", () => ({ CodeBlock: () => <div /> }));
vi.mock("../common/UserAvatar", () => ({ UserAvatar: () => <div /> }));
vi.mock("../common/AgentAvatar", () => ({ AgentAvatar: () => <div /> }));
vi.mock("../common/Tooltip", () => ({
  Tooltip: ({ children }: { children?: React.ReactNode }) => <>{children}</>,
}));
vi.mock("../editor/markdownLinkResolver", () => ({
  handleChatMarkdownLinkClick: vi.fn(),
  markdownUrlTransform: (x: string) => x,
}));

function userRow(seq: number, body: string, extra?: Record<string, unknown>): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: "user",
    kind: "user_message",
    body,
    to: "agent",
    meta: { message_id: `u${seq}`, ...(extra ?? {}) },
  };
}

function askCardRow(seq: number): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: "agent",
    kind: "interaction_request",
    body: "",
    to: "user",
    meta: {
      message_id: `card${seq}`,
      agent_session_id: "exec-9",
      interaction: {
        kind: "ask_question",
        request_id: "q1",
        question: "Pick one",
        options: ["A", "B"],
      },
    },
  };
}

describe("AgentChatView unified-card wiring", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    inFlightMap = {};
    vi.stubGlobal(
      "fetch",
      vi.fn().mockRejectedValue(new Error("no network in test")),
    );
  });

  it("renders an unanswered ask_question ONLY through the unified card, not a standalone row", () => {
    windows = {
      [SESSION]: {
        older: [],
        tail: [userRow(1, "hello there"), askCardRow(2)],
        tailLoaded: 2,
        total: 2,
        loadingOlder: false,
      },
    };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    // The durable user_message paints as a normal bubble.
    expect(screen.getByText("hello there")).toBeTruthy();

    // The interaction_request row is suppressed: its question never renders
    // as a standalone line anywhere in the stream.
    expect(screen.queryByText("Pick one")).toBeNull();

    // …and is instead carried by the unified status card, bound to the card's
    // kind + message id (the answer channel's in_reply_to target).
    const card = screen.getByTestId("status-card");
    expect(card.getAttribute("data-kind")).toBe("ask_question");
    expect(card.getAttribute("data-msgid")).toBe("card2");
  });

  it("drops the card and leaves no orphan row once the card is answered", () => {
    windows = {
      [SESSION]: {
        older: [],
        tail: [
          userRow(1, "hello there"),
          askCardRow(2),
          userRow(3, "B", { in_reply_to: "card2" }),
        ],
        tailLoaded: 3,
        total: 3,
        loadingOlder: false,
      },
    };
    render(<AgentChatView agentId={AGENT} sessionId={SESSION} />);

    // No live card once every projection is answered (inFlight absent too).
    expect(screen.queryByTestId("status-card")).toBeNull();
    // No "已回答" / question orphan — the answered card row paints nothing.
    expect(screen.queryByText("Pick one")).toBeNull();
    // The answer itself is just a normal user bubble.
    expect(screen.getByText("B")).toBeTruthy();
  });
});
