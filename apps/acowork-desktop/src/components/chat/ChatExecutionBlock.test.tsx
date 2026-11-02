// @vitest-environment jsdom
/**
 * ChatExecutionBlock — the chat-side rendering of one `execution_round`
 * reference row (ADR-106 D3/D8/D9/D10).
 *
 * Why this file exists: the block is where the round's content and its pending
 * decision now live, so the two failure modes it must not reintroduce are
 *   1. inventing liveness — drawing "still running" from anything other than a
 *      `pending` block read, which lets an unreachable Runtime look like a dead
*      node still working (D9);
 *   2. answering through the chat — the decision must post to the EXECUTION
 *      session's own write entry, where the owner check already runs (D8).
 *
 * The block store is mocked at its read hook: these tests are about what one
 * entry implies for the UI and for the call that closes the round, not about
 * batching (pinned in chatBlockStore's own tests).
 */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi, beforeEach } from "vitest";
import { ChatExecutionBlock } from "./ChatExecutionBlock";
import type { ChatBlockEntry } from "../../stores/chatBlockStore";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const reload = vi.fn();
const useChatBlock = vi.fn();
const resolveQuestion = vi.fn();

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../stores/chatBlockStore", () => ({
  useChatBlock: (...args: unknown[]) => useChatBlock(...args),
  useChatBlockStore: (selector: (s: { reload: typeof reload }) => unknown) =>
    selector({ reload }),
}));
vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector({}),
    { getState: () => ({ resolveQuestion }) },
  ),
}));
// Markdown rendering is AskQuestionCard/cardMarkdown's job; here the component
// is a slot marker so the assertion is "the prompt went through the renderer".
vi.mock("./ChatMarkdown", () => ({
  ChatMarkdown: ({ content }: { content: string }) => (
    <div data-testid="md">{content}</div>
  ),
}));
vi.mock("../../lib/session-control", () => ({
  sendApproval: vi.fn(async () => {}),
  sendAnswer: vi.fn(async () => {}),
}));

import { sendAnswer, sendApproval } from "../../lib/session-control";

const REF = "ag|s1|r1";

function refRow(taskId?: string): ChatMessageRow {
  return {
    seq: 3,
    ts: 1,
    from: "ag",
    kind: "execution_round",
    body: "",
    to: "u",
    meta: {
      message_id: `round:${REF}`,
      agent_instance_id: "ag",
      agent_session_id: "s1",
      round_id: "r1",
      task_id: taskId,
    },
  };
}

function entryOf(over: Partial<ChatBlockEntry>): ChatBlockEntry {
  return {
    ref: REF,
    agent_instance_id: "ag",
    agent_session_id: "s1",
    round_id: "r1",
    status: "ok",
    projection: { round_id: "r1", terminal: true, status: "completed", answer: null, card: null },
    ...over,
  };
}

function renderBlock(
  over: Partial<ChatBlockEntry> | null,
  props: Partial<React.ComponentProps<typeof ChatExecutionBlock>> = {},
) {
  useChatBlock.mockReturnValue(over ? entryOf(over) : undefined);
  return render(
    <ChatExecutionBlock row={refRow()} chatSessionId="c1" {...props} />,
  );
}

describe("ChatExecutionBlock", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("states 'in progress' only on positive evidence: a pending read", () => {
    renderBlock({ status: "pending", projection: { round_id: "r1", terminal: false, status: "in_progress" } });
    expect(screen.getByText("chatView.block.inProgress")).toBeTruthy();
    expect(screen.getByText("chatView.block.working")).toBeTruthy();
    expect(document.querySelector(".animate-spin")).toBeTruthy();
  });

  it("an unreachable read is an outage with a retry, never a run in flight (D9)", async () => {
    renderBlock({ status: "unreachable", projection: undefined, detail: "connection refused" });
    expect(screen.getByText("chatView.block.unreachable")).toBeTruthy();
    // No spinner, no in-progress copy: the block does not pretend.
    expect(document.querySelector(".animate-spin")).toBeNull();
    expect(screen.queryByText("chatView.block.inProgress")).toBeNull();
    fireEvent.click(screen.getByText("chatView.block.retry"));
    expect(reload).toHaveBeenCalledWith("c1", [REF]);
  });

  it("no coordinates, no read: a placeholder line, not an empty transcript", () => {
    useChatBlock.mockReturnValue(undefined);
    render(
      <ChatExecutionBlock
        row={{ ...refRow(), meta: { ...refRow().meta, round_id: undefined } }}
        chatSessionId="c1"
      />,
    );
    expect(screen.getByText("chatView.block.notFound")).toBeTruthy();
  });

  it("an explicit hole stays explicit (not_found)", () => {
    renderBlock({ status: "not_found", projection: undefined });
    expect(screen.getByText("chatView.block.notFound")).toBeTruthy();
  });

  it("the round's answer renders as markdown, with the author and task in the header", () => {
    renderBlock(
      { projection: { round_id: "r1", terminal: true, status: "completed", answer: { body: "done", ts: "1" } } },
      { authorName: "Ada", className: "px-4" },
    );
    expect(screen.getByTestId("md").textContent).toBe("done");
    expect(screen.getByText("Ada")).toBeTruthy();
    expect(screen.getByText("chatView.block.completed")).toBeTruthy();
  });

  it("a pending approval is answered against the execution session, then cleared locally (D8)", async () => {
    renderBlock({
      projection: {
        round_id: "r1",
        terminal: false,
        status: "waiting",
        card: { kind: "approval", request_id: "a1", prompt: "Approve `bash`?" },
      },
    });
    // The card prompt is model output, so it renders as markdown like every
    // other chat surface.
    expect(screen.getByTestId("md").textContent).toBe("Approve `bash`?");
    fireEvent.click(screen.getByText("chatView.cardApprove"));
    await waitFor(() =>
      expect(sendApproval).toHaveBeenCalledWith("ag", "s1", { request_id: "a1", approved: true }),
    );
    expect(resolveQuestion).toHaveBeenCalledWith("ag", "a1", "s1");
  });

  it("an ask_question takes the picked label through the execution write entry", async () => {
    renderBlock({
      projection: {
        round_id: "r1",
        terminal: false,
        status: "waiting",
        card: { kind: "ask_question", request_id: "q1", prompt: "Pick one", options: ["A", "B"] },
      },
    });
    fireEvent.click(screen.getByText("A"));
    await waitFor(() =>
      expect(sendAnswer).toHaveBeenCalledWith("ag", "s1", { request_id: "q1", answer: "A" }),
    );
  });

  it("a viewer who is not the answerer sees the question and no buttons (D10)", () => {
    renderBlock(
      {
        projection: {
          round_id: "r1",
          terminal: false,
          status: "waiting",
          card: { kind: "ask_question", request_id: "q1", prompt: "Pick one", options: ["A", "B"] },
        },
      },
      { canAnswer: false },
    );
    expect(screen.getByText("Pick one")).toBeTruthy();
    expect(screen.getByText("chatView.block.answeredByOwner")).toBeTruthy();
    expect(screen.queryByText("A")).toBeNull();
  });

  it("a pause card continues through the composer, addressed to the reference row (D6)", async () => {
    const onReplyToRound = vi.fn();
    renderBlock(
      {
        status: "pending",
        projection: {
          round_id: "r1",
          terminal: false,
          status: "waiting",
          card: { kind: "pause", request_id: "p1", prompt: "Blocked on you" },
        },
      },
      { onReplyToRound },
    );
    fireEvent.click(screen.getByText("chatView.cardContinue"));
    expect(onReplyToRound).toHaveBeenCalledWith(`round:${REF}`, "Blocked on you");
    // A pause has no execution-side request to resolve.
    expect(sendAnswer).not.toHaveBeenCalled();
  });

  it("wears neutral chrome: a hairline shell, no highlight color anywhere", () => {
    const { container } = renderBlock({
      projection: {
        round_id: "r1",
        terminal: true,
        status: "completed",
        answer: { body: "done", ts: "1" },
        card: { kind: "approval", request_id: "a1", prompt: "Approve?" },
      },
    });
    const shell = container.querySelector("div[class*='rounded-lg'][class*='border']") as HTMLElement;
    expect(shell.className).toContain("border-hairline");
    expect(shell.className).not.toContain("--color-accent");
    expect(shell.querySelector("header")?.className).not.toContain("--color-accent");
  });

  it("a terminal failure paints its outcome copy and no run chip is invented", () => {
    renderBlock({
      projection: { round_id: "r1", terminal: true, status: "failed", answer: null },
    });
    expect(screen.getByText("chatView.notice.failed")).toBeTruthy();
    expect(document.querySelector(".text-danger")).toBeTruthy();
    // No answer body and not in progress: nothing is invented for the content.
    expect(screen.queryByTestId("md")).toBeNull();
    expect(screen.queryByText("chatView.block.working")).toBeNull();
  });
});
