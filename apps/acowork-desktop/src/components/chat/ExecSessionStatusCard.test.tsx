import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ExecSessionStatusCard } from "./ExecSessionStatusCard";

const AGENT = "agent-1";
const EXEC = "exec-1";

// ── live-mirror fixture ─────────────────────────────────────────────
// The card mirrors chatStore.sessionStates[execSid]; drive it with a mutable
// object so each test sets only the slices it cares about.
const mocks = vi.hoisted(() => ({
  t: (key: string, params?: Record<string, unknown>): string => {
    const dict: Record<string, string> = {
      "chatView.statusCard.untitled": "Untitled run",
      "chatView.statusCard.detail": "Detail",
      "chatView.statusCard.labelStatus": "Status",
      "chatView.statusCard.labelStep": "Step",
      "chatView.statusCard.labelModel": "Model",
      "chatView.statusCard.labelWorkspace": "Workspace",
      "chatView.statusCard.workspaceAgentHome": "Agent Home",
      "chatView.statusCard.status.running": "Running",
      "askQuestionCard.other": "Other",
      "askQuestionCard.submit": "Submit",
      "askQuestionCard.submitted": "Submitted",
      "askQuestionCard.placeholder": "Type an answer",
      "chatView.queued": "Queued",
    };
    if (key === "chatView.statusCard.stepTodo" && params) {
      return `${String(params.index)}/${String(params.total)}: ${String(params.content)}`;
    }
    return dict[key] ?? key;
  },
}));

let sessionState: Record<string, unknown>;
let chatState: Record<string, unknown>;

function baseSessionState() {
  return {
    sessionStatus: { status: "running" },
    model: "gpt-x",
    provider: "openai",
    todos: [],
    pendingApproval: {},
    serverError: null,
    pendingQuestions: [],
    toolProgress: {},
  };
}

function resetStores() {
  sessionState = baseSessionState();
  chatState = {
    agentStates: { [AGENT]: { sessionStates: { [EXEC]: sessionState } } },
    fetchSessionConfig: vi.fn().mockResolvedValue(undefined),
    resolveApprovalByToolCallId: vi.fn(),
    clearServerError: vi.fn(),
  };
}

vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector(chatState),
    { getState: () => chatState },
  ),
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (state: unknown) => unknown) =>
    selector({
      agents: {
        [AGENT]: {
          meta: { display_name: "Agent A", name: "agent-a" },
          sessions: [{ session_id: EXEC, title: "My run" }],
        },
      },
    }),
}));

vi.mock("../../stores/workspaceStore", () => ({
  useWorkspaceStore: (selector: (state: unknown) => unknown) =>
    selector({ sessionWorkspaceMap: {}, workspaces: [] }),
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: mocks.t }),
}));

// Keep the test hermetic — the avatar pulls in settings/asset stores the card
// does not otherwise touch, and the brain icon is a pure svg.
vi.mock("../common/AgentAvatar", () => ({
  AgentAvatar: () => <div data-testid="agent-avatar" />,
}));

vi.mock("../../lib/session-control", () => ({
  sendApproval: vi.fn().mockResolvedValue(undefined),
  continueSession: vi.fn().mockResolvedValue(undefined),
}));

function renderCard(props: Partial<Parameters<typeof ExecSessionStatusCard>[0]> = {}) {
  return render(
    <ExecSessionStatusCard agentId={AGENT} execSid={EXEC} {...props} />,
  );
}

describe("ExecSessionStatusCard", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    resetStores();
  });

  it("shows the brain-icon header with the exec session title, not a repeated title row", () => {
    renderCard({ onDrill: vi.fn() });

    // Header names the run by its title.
    expect(screen.getByText("My run")).toBeTruthy();
    // The detail block carries the execution status (label + value), but no
    // separate "title" row is duplicated below the header.
    expect(screen.getByText("Status")).toBeTruthy();
    expect(screen.queryByText("chatView.statusCard.labelTitle")).toBeNull();
  });

  it("drops the step row entirely when there is no in-progress todo", () => {
    renderCard();
    // No todos → no step row, and no fallback label either.
    expect(screen.queryByText("Step")).toBeNull();
  });

  it("renders the step row only for an in-progress todo", () => {
    sessionState.todos = [
      { content: "Write the report", status: "in_progress" },
      { content: "Summarize", status: "pending" },
    ];
    renderCard();
    expect(screen.getByText("Step")).toBeTruthy();
    expect(screen.getByText("1/2: Write the report")).toBeTruthy();
  });

  it("projects a pending ask_question with title + rich options and forwards the answer", async () => {
    const onAnswerCard = vi.fn();
    sessionState.pendingQuestions = [
      {
        type: "ask_question",
        request_id: "q1",
        agent_id: AGENT,
        title: "Choose a branch",
        question: "Which strategy should I take?",
        options: [
          { label: "Conservative", description: "safe, slow" },
          { label: "Aggressive", description: "fast, risky" },
        ],
        timeout_seconds: 300,
      },
    ];
    renderCard({
      card: { kind: "ask_question", messageId: "m1" },
      onAnswerCard,
    });

    // Title + question + option label + its description are all rendered.
    expect(screen.getByText("Choose a branch")).toBeTruthy();
    expect(screen.getByText("Which strategy should I take?")).toBeTruthy();
    expect(screen.getByText("Conservative")).toBeTruthy();
    expect(screen.getByText("— safe, slow")).toBeTruthy();

    // Pick the second option and submit → forwarded with immediate=true.
    const radios = screen.getAllByRole("radio");
    fireEvent.click(radios[1]!);
    fireEvent.click(screen.getByText("Submit"));

    await waitFor(() =>
      expect(onAnswerCard).toHaveBeenCalledWith("m1", "Aggressive", true),
    );
  });
});
