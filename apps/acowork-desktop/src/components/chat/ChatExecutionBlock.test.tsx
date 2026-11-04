// @vitest-environment jsdom
/**
 * ChatExecutionBlock — the chat-side rendering of ONE execution session's rounds
 * as one collapsible block (ADR-106 D3/D4/D8/D9/D10/D13; the presentation
 * contract in design 27 §5.5).
 *
 * Why this file exists — the failure modes the block must not reintroduce:
 *   1. inventing liveness — drawing "still running" from anything other than a
 *      `pending` block read. `session_state` refines what the header says; it
 *      never substitutes for the read (D9), and a dead node must not look busy;
 *   2. answering through the chat — the decision must post to the EXECUTION
 *      session's own write entry, where the owner check already runs (D8);
 *   3. one card per reference row — a session's rounds are one object, so the
 *      block is the unit of the transcript, not the row (D3/D4);
 *   4. inventing chrome the surfaces do not have — the block is `ExploreBlock`'s
 *      grammar (pill header, `border-l-2` list, row + two panels), and the two
 *      panels wear the transcript's own bubble colours: the highlight wash is
 *      the human's line, the agent bubble fill is the answer, and the highlight
 *      paints no border and no title.
 *
 * The block store is mocked at its read hook: these tests are about what an
 * entry implies for the UI and for the call that closes the round, not about
 * batching (pinned in chatBlockStore's own tests).
 */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi, beforeEach } from "vitest";
import { ChatExecutionBlock } from "./ChatExecutionBlock";
import type { ChatBlockEntry } from "../../lib/chatBlocks";
import type { ChatMessageRow } from "../../stores/chatSessionStore";
import type { RoundItem, RunBlock } from "./chatTranscriptGroups";

const reload = vi.fn();
const useChatBlocks = vi.fn();
const resolveQuestion = vi.fn();
// Hoisted: the mock factories run before the module body, and the session_state
// test has to swap what the chat store selector is handed.
const store = vi.hoisted(() => ({ chat: { agentStates: {} } as any }));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../stores/chatBlockStore", () => ({
  useChatBlocks: (...args: unknown[]) => useChatBlocks(...args),
  useChatBlockStore: (selector: (s: { reload: typeof reload }) => unknown) =>
    selector({ reload }),
}));
vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector(store.chat),
    { getState: () => ({ resolveQuestion }) },
  ),
}));
// Markdown rendering is ChatMarkdown's own job; here the component is a slot
// marker so the assertion is "this text went through the renderer".
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
import { useChatSessionStore } from "../../stores/chatSessionStore";

const REF1 = "ag|s1|r1";
const REF2 = "ag|s1|r2";

function roundRow(
  seq: number,
  roundId: string,
  taskId?: string,
): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: "ag",
    kind: "execution_round",
    body: "",
    to: "u",
    meta: {
      message_id: `round:ag|s1|${roundId}`,
      agent_instance_id: "ag",
      agent_session_id: "s1",
      round_id: roundId,
      ...(taskId ? { task_id: taskId } : {}),
    },
  };
}

function triggerRow(seq: number, body: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: "u",
    kind: "user_message",
    body,
    to: "ag",
    meta: { message_id: `u${seq}` },
  };
}

function roundOf(roundId: string, seq: number): RoundItem {
  return {
    ref: `ag|s1|${roundId}`,
    roundRow: roundRow(seq, roundId),
    showUserLine: true,
  };
}

function blockOf(rounds: RoundItem[]): RunBlock {
  return {
    kind: "run",
    instanceId: "ag",
    execSessionId: "s1",
    rounds,
    anchorRow: rounds[rounds.length - 1]!.roundRow,
  };
}

function entryOf(over: Partial<ChatBlockEntry>): ChatBlockEntry {
  return {
    ref: REF1,
    agent_instance_id: "ag",
    agent_session_id: "s1",
    round_id: "r1",
    status: "ok",
    projection: {
      round_id: "r1",
      terminal: true,
      status: "completed",
      answer: null,
      card: null,
    },
    ...over,
  };
}

/** Entries by ref; a ref absent from the map is the loading hole. */
function renderBlock(
  entries: Record<string, ChatBlockEntry | undefined>,
  props: Partial<React.ComponentProps<typeof ChatExecutionBlock>> = {},
  block: RunBlock = blockOf([roundOf("r1", 3)]),
) {
  useChatBlocks.mockReturnValue(entries);
  return render(<ChatExecutionBlock block={block} chatSessionId="c1" {...props} />);
}

/** The collapse header's own copy. The count and the state sit in one span with
 *  the parentheses as separate text nodes (`ExploreBlock`'s shape), so the
 *  assertion reads the header element, not a whole-text match. */
function headerText(container: HTMLElement): string {
  return container.querySelector("[aria-expanded]")?.textContent ?? "";
}

describe("ChatExecutionBlock", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    store.chat = { agentStates: {} };
  });

  it("one execution session is one block: its rounds are rows under one header", () => {
    const r1 = entryOf({
      status: "ok",
      projection: { round_id: "r1", terminal: true, status: "completed", answer: { body: "first", ts: "1" } },
    });
    const r2 = entryOf({
      ref: REF2,
      round_id: "r2",
      status: "ok",
      projection: { round_id: "r2", terminal: true, status: "completed", answer: { body: "second", ts: "2" } },
    });
    const { container } = renderBlock(
      { [REF1]: r1, [REF2]: r2 },
      {},
      blockOf([roundOf("r1", 2), roundOf("r2", 4)]),
    );
    // One collapse header for the session, not one per reference row.
    expect(document.querySelectorAll("[aria-expanded]")).toHaveLength(1);
    expect(headerText(container)).toContain("chatView.block.roundCount");
    expect(screen.getAllByText("chatView.block.roundN")).toHaveLength(2);
  });

  it("a folded round renders its own question line, addressable by its seq", () => {
    const round = { ...roundOf("r1", 3), triggerRow: triggerRow(1, "修复分页不满屏") };
    const { container } = renderBlock(
      { [REF1]: entryOf({}) },
      {},
      blockOf([round]),
    );
    // The row's summary line and the opened question panel carry the same
    // sentence, so the assertion is on the panel the seq addresses: a locate on
    // the question row lands on text that is really there.
    const question = container.querySelector('[data-seq="1"]');
    expect(question?.textContent).toContain("修复分页不满屏");
    // ...and the reference row the round itself is stays addressable too.
    expect(container.querySelector('[data-seq="3"]')).toBeTruthy();
  });

  /** ADR-106 presentation-contract amendment: the opener round's sentence is
   *  already on the transcript as its own bubble, so the block repeats neither
   *  the panel nor the raw text — its line leads with the router's brief. */
  it("an opener round leads with the brief and renders no question panel", () => {
    const base = roundRow(3, "r1");
    const opener: RoundItem = {
      ref: REF1,
      roundRow: {
        ...base,
        meta: { ...base.meta, opened_new: true, assignment_brief: "Fix the pagination bug" },
      },
      triggerRow: triggerRow(1, "修复分页不满屏"),
      showUserLine: false,
      questionOnTranscript: true,
    };
    const { container } = renderBlock(
      {
        [REF1]: entryOf({
          projection: {
            round_id: "r1",
            terminal: true,
            status: "completed",
            user: { body: "Fix the pagination bug\n\n[user message]\n修复分页不满屏", ts: "1", attachments: [] },
            answer: { body: "done", ts: "2" },
            card: null,
          },
        }),
      },
      {},
      blockOf([opener]),
    );
    // The row's summary is the brief — what the session actually received.
    const row = container.querySelector('[data-seq="3"]');
    expect(row?.textContent).toContain("Fix the pagination bug");
    // The raw sentence is not painted a second time inside the block: no
    // question panel carries the trigger's seq (that row stands on its own).
    expect(container.querySelector('pre[data-seq="1"]')).toBeNull();
  });

  it("states 'in progress' only on positive evidence: a pending read", () => {
    renderBlock({
      [REF1]: entryOf({
        status: "pending",
        projection: { round_id: "r1", terminal: false, status: "in_progress" },
      }),
    });
    expect(screen.getAllByText("chatView.block.inProgress").length).toBeGreaterThan(0);
    expect(document.querySelector(".animate-spin")).toBeTruthy();
  });

  it("an unreachable read is an outage with a retry, never a run in flight (D9)", () => {
    renderBlock({
      [REF1]: entryOf({ status: "unreachable", projection: undefined, detail: "connection refused" }),
    });
    expect(screen.getByText("chatView.block.unreachable")).toBeTruthy();
    expect(document.querySelector(".animate-spin")).toBeNull();
    expect(screen.queryByText("chatView.block.inProgress")).toBeNull();
    fireEvent.click(screen.getByText("chatView.block.retry"));
    expect(reload).toHaveBeenCalledWith("c1", [REF1]);
  });

  it("an explicit hole stays explicit (not_found)", () => {
    renderBlock({ [REF1]: entryOf({ status: "not_found", projection: undefined }) });
    expect(screen.getByText("chatView.block.notFound")).toBeTruthy();
  });

  it("a round whose read has not landed says it is loading, without inventing content", () => {
    renderBlock({});
    expect(screen.getByText("chatView.block.loading")).toBeTruthy();
    expect(screen.queryByTestId("md")).toBeNull();
  });

  it("the answer renders as markdown, with the author and the task coordinate in the header", () => {
    const round = {
      ...roundOf("r1", 3),
      roundRow: roundRow(3, "r1", "t-083ae242"),
      triggerRow: triggerRow(2, "go"),
    };
    const { container } = renderBlock(
      {
        [REF1]: entryOf({
          projection: {
            round_id: "r1",
            terminal: true,
            status: "completed",
            answer: { body: "done", ts: "1" },
          },
        }),
      },
      { authorName: "Ada", showHeader: true },
      blockOf([round]),
    );
    expect(screen.getByTestId("md").textContent).toBe("done");
    expect(screen.getByText("Ada")).toBeTruthy();
    expect(headerText(container)).toContain("chatView.block.completed");
    // D13: the task rides the round, and the header is where it is stated.
    expect(screen.getByText("t-083ae242")).toBeTruthy();
  });

  it("a pending approval is answered against the execution session, then cleared locally (D8)", async () => {
    renderBlock({
      [REF1]: entryOf({
        status: "pending",
        projection: {
          round_id: "r1",
          terminal: false,
          status: "waiting",
          card: { kind: "approval", request_id: "a1", prompt: "Approve `bash`?" },
        },
      }),
    });
    // The card prompt is model output, so it renders as markdown like every
    // other chat surface.
    expect(screen.getByTestId("md").textContent).toBe("Approve `bash`?");
    fireEvent.click(screen.getByText("chatView.cardApprove"));
    await waitFor(() =>
      expect(sendApproval).toHaveBeenCalledWith("ag", "s1", {
        request_id: "a1",
        approved: true,
      }),
    );
    expect(resolveQuestion).toHaveBeenCalledWith("ag", "a1", "s1");
  });

  it("an ask_question takes the picked label through the execution write entry", async () => {
    renderBlock({
      [REF1]: entryOf({
        status: "pending",
        projection: {
          round_id: "r1",
          terminal: false,
          status: "waiting",
          card: { kind: "ask_question", request_id: "q1", prompt: "Pick one", options: ["A", "B"] },
        },
      }),
    });
    fireEvent.click(screen.getByText("A"));
    await waitFor(() =>
      expect(sendAnswer).toHaveBeenCalledWith("ag", "s1", {
        request_id: "q1",
        answer: "A",
      }),
    );
  });

  it("a viewer who is not the answerer sees the question and no buttons (D10)", () => {
    renderBlock(
      {
        [REF1]: entryOf({
          status: "pending",
          projection: {
            round_id: "r1",
            terminal: false,
            status: "waiting",
            card: { kind: "ask_question", request_id: "q1", prompt: "Pick one", options: ["A", "B"] },
          },
        }),
      },
      { canAnswer: false },
    );
    expect(screen.getByText("Pick one")).toBeTruthy();
    expect(screen.getByText("chatView.block.answeredByOwner")).toBeTruthy();
    expect(screen.queryByText("A")).toBeNull();
  });

  it("a pause card continues through the composer, addressed to the reference row (D6)", () => {
    const onReplyToRound = vi.fn();
    renderBlock(
      {
        [REF1]: entryOf({
          status: "pending",
          projection: {
            round_id: "r1",
            terminal: false,
            status: "waiting",
            card: { kind: "pause", request_id: "p1", prompt: "Blocked on you" },
          },
        }),
      },
      { onReplyToRound },
    );
    fireEvent.click(screen.getByText("chatView.cardContinue"));
    expect(onReplyToRound).toHaveBeenCalledWith("round:ag|s1|r1", "Blocked on you");
    // A pause has no execution-side request to resolve.
    expect(sendAnswer).not.toHaveBeenCalled();
  });

  it("the header carries the session's live state without inventing liveness", () => {
    store.chat = {
      agentStates: {
        ag: { sessionStates: { s1: { sessionStatus: { status: "tool_executing" } } } },
      },
    };
    // The session's own activity is the freshest fact and outranks the block's
    // settled rounds (a session can be busy with a round this Chat holds no row
    // for yet), but it may not manufacture liveness: the read is TERMINAL, so no
    // spinner and no "in progress" copy ride in on it (D9).
    const { container } = renderBlock({ [REF1]: entryOf({}) });
    expect(headerText(container)).toContain("chatView.block.state.toolExecuting");
    expect(document.querySelector(".animate-spin")).toBeNull();
    expect(screen.queryByText("chatView.block.inProgress")).toBeNull();
  });

  it("an idle session lets the block's own outcome speak", () => {
    store.chat = {
      agentStates: {
        ag: { sessionStates: { s1: { sessionStatus: { status: "idle" } } } },
      },
    };
    const { container } = renderBlock({ [REF1]: entryOf({}) });
    expect(headerText(container)).toContain("chatView.block.completed");
    expect(headerText(container)).not.toContain("chatView.block.state.");
  });

  it("the drill keeps its own chrome and sits above the block", () => {
    const onJumpToExec = vi.fn();
    const { container } = renderBlock(
      { [REF1]: entryOf({}) },
      { onJumpToExec },
    );
    const drill = screen.getByText("chatView.viewRun");
    const pill = container.querySelector("[aria-expanded]") as HTMLElement;
    expect(onJumpToExec).not.toHaveBeenCalled();
    fireEvent.click(drill);
    expect(onJumpToExec).toHaveBeenCalledWith("ag", "s1");
    // Document order: the drill precedes the collapse header it is not part of.
    expect(
      pill.compareDocumentPosition(drill) & Node.DOCUMENT_POSITION_PRECEDING,
    ).toBeTruthy();
  });

  it("a long run keeps its older rounds inside the block, one click away", () => {
    const entries: Record<string, ChatBlockEntry | undefined> = {};
    const rounds: RoundItem[] = [];
    for (let n = 1; n <= 6; n++) {
      const round = roundOf(`r${n}`, n);
      entries[round.ref] = entryOf({
        ref: round.ref,
        round_id: `r${n}`,
        projection: { round_id: `r${n}`, terminal: true, status: "completed", answer: { body: `a${n}`, ts: `${n}` } },
      });
      rounds.push(round);
    }
    renderBlock(entries, {}, blockOf(rounds));
    // Two recent rounds visible, the rest folded in the block itself.
    expect(screen.getAllByText("chatView.block.roundN")).toHaveLength(2);
    fireEvent.click(screen.getByText("chatView.block.olderRounds"));
    // One step reveals the whole backlog of a six-round run, and the affordance
    // disappears once there is nothing older left to show.
    expect(screen.getAllByText("chatView.block.roundN")).toHaveLength(6);
    expect(screen.queryByText("chatView.block.olderRounds")).toBeNull();
  });

  it("wears ExploreBlock's chrome: pill header, ruled list, no invented card shell", () => {
    const { container } = renderBlock({
      [REF1]: entryOf({
        projection: {
          round_id: "r1",
          terminal: true,
          status: "completed",
          answer: { body: "done", ts: "1" },
          card: { kind: "approval", request_id: "a1", prompt: "Approve?" },
        },
      }),
    });
    const pill = container.querySelector("[aria-expanded]") as HTMLElement;
    expect(pill.className).toContain("bg-zinc-100/60");
    // The highlight color is a button background, never a border or a title.
    expect(pill.className).not.toContain("--color-accent");
    expect(document.querySelector("[class*='border-l-2']")).toBeTruthy();
    expect(container.querySelector("[class*='border-hairline']")).toBeNull();
  });

  it("paints the pair in the transcript's own bubble colours", () => {
    const round = { ...roundOf("r1", 3), triggerRow: triggerRow(2, "go") };
    const { container } = renderBlock(
      {
        [REF1]: entryOf({
          projection: {
            round_id: "r1",
            terminal: true,
            status: "completed",
            answer: { body: "done", ts: "1" },
          },
        }),
      },
      {},
      blockOf([round]),
    );
    // The user message is the highlighted bubble in the list, so the block's
    // question line has to be the highlighted panel here.
    const question = container.querySelector("pre[data-seq='2']") as HTMLElement;
    expect(question.className).toContain("--color-accent");
    // A reply wearing the highlight read as the speaker who is not the reader:
    // the answer takes the agent bubble's fill and nothing of the accent.
    const answer = screen.getByTestId("md").parentElement as HTMLElement;
    expect(answer.className).toContain("bg-chat-bubble");
    expect(answer.className).not.toContain("--color-accent");
  });

  it("a terminal failure paints its outcome and invents no run", () => {
    renderBlock({
      [REF1]: entryOf({
        projection: { round_id: "r1", terminal: true, status: "failed", answer: null },
      }),
    });
    expect(screen.getByText("chatView.notice.failed")).toBeTruthy();
    expect(screen.queryByTestId("md")).toBeNull();
    expect(document.querySelector(".animate-spin")).toBeNull();
  });
});

/**
 * A block's header is its claim about ONE session, and the reported failure was
 * that the claim lied twice over (t-1b7bdb21):
 *   1. the human's own sentence disappeared — the round that owns a folded chat
 *      question sat outside the recent-rounds window, so the only place the
 *      sentence was ever drawn was hidden, and the visible rows looked like the
 *      agent answering questions nobody had asked in this Chat;
 *   2. the state word answered "is ANY round of this session pending" instead of
 *      "how did the round you are reading end" — a run parked on a card read as
 *      "still executing" (it is waiting for the human), a settled newest round
 *      still spun while an older read was pending, and a hole said nothing at
 *      all.
 */
describe("ChatExecutionBlock — what a session's header claims (t-1b7bdb21)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    store.chat = { agentStates: {} };
  });

  const REF3 = "ag|s1|r3";

  /** A round that owns a folded chat question. */
  function askedRound(roundId: string, seq: number, questionSeq: number, question: string): RoundItem {
    return { ...roundOf(roundId, seq), triggerRow: triggerRow(questionSeq, question) };
  }
  function completed(ref: string, body: string): ChatBlockEntry {
    return entryOf({
      ref,
      round_id: ref.split("|")[2],
      status: "ok",
      projection: { round_id: ref.split("|")[2]!, terminal: true, status: "completed", answer: { body, ts: "1" } },
    });
  }

  it("never hides the round that carries this Chat's own question", () => {
    // The live shape: one chat question opened round 1, then the human kept
    // nudging from the execution surface, so rounds 2 and 3 answer sentences the
    // Chat never saw. Hiding round 1 hid the human's sentence from the whole
    // transcript — a folded row has no other place to be drawn.
    const { container } = renderBlock(
      {
        [REF1]: completed(REF1, "answer one"),
        [REF2]: completed(REF2, "answer two"),
        [REF3]: completed(REF3, "answer three"),
      },
      {},
      blockOf([
        askedRound("r1", 2, 1, "修复分页不满屏"),
        roundOf("r2", 4),
        roundOf("r3", 6),
      ]),
    );

    // Every round is rendered, so nothing is behind "earlier rounds".
    expect(screen.getAllByText("chatView.block.roundN")).toHaveLength(3);
    expect(screen.queryByText("chatView.block.olderRounds")).toBeNull();
    // The question is on screen in the row of the round it opened: a closed
    // round carries its sentence on the one-line label.
    expect(container.querySelector('[data-seq="2"]')?.textContent).toContain(
      "修复分页不满屏",
    );
  });

  it("keeps answering reachable: a round parked on a card is never windowed away", () => {
    const card: ChatBlockEntry = entryOf({
      ref: REF1,
      status: "pending",
      projection: {
        round_id: "r1",
        terminal: false,
        status: "waiting",
        answer: { body: "need approval", ts: "1" },
        card: { kind: "approval", request_id: "q1", prompt: "Approve `bash`?" },
      },
    });
    renderBlock(
      { [REF1]: card, [REF2]: completed(REF2, "b"), [REF3]: completed(REF3, "c") },
      {},
      blockOf([roundOf("r1", 2), roundOf("r2", 4), roundOf("r3", 6)]),
    );
    // D8: an unanswered card must stay on screen — an answerable thing behind a
    // "show earlier" click is not answerable.
    expect(screen.getByText("Approve `bash`?")).toBeTruthy();
  });

  it("a settled block reads Done even when an older round's read still says pending", () => {
    const { container } = renderBlock(
      {
        [REF1]: entryOf({ ref: REF1, status: "pending" }),
        [REF2]: completed(REF2, "the newest answer"),
      },
      {},
      blockOf([roundOf("r1", 2), roundOf("r2", 4)]),
    );
    // The newest round is the block's claim; an older read that never resolves
    // must not keep the whole run looking live.
    expect(headerText(container)).toContain("chatView.block.completed");
    expect(container.querySelector(".animate-spin")).toBeNull();
  });

  it("a round parked on a card reads as waiting for the human, not as running", () => {
    const { container } = renderBlock(
      {
        [REF1]: entryOf({
          ref: REF1,
          status: "pending",
          projection: {
            round_id: "r1",
            terminal: false,
            status: "waiting",
            card: { kind: "approval", request_id: "q1", prompt: "Approve?" },
          },
        }),
      },
    );
    expect(headerText(container)).toContain("chatView.notice.waiting");
    expect(container.querySelector(".animate-spin")).toBeNull();
  });

  it("a newest read that is a hole says so instead of claiming a state", () => {
    const { container } = renderBlock({
      [REF1]: entryOf({ ref: REF1, status: "not_found", projection: undefined }),
    });
    // D9: "in progress" is a positive claim; a missing read is not evidence of it.
    expect(headerText(container)).toContain("chatView.block.unknown");
    expect(headerText(container)).not.toContain("chatView.block.inProgress");
  });
});

/**
 * The block's in-place reply box (ADR-106 revision 2026-11-04). It lives OUTSIDE
 * the collapse body, so a run that is folded away is still continuable in place.
 * The three behaviours the IM habit demands, and the one wire contract:
 *   - reveal on hover, but a held draft or focus NEVER collapses it (typed
 *     words lost to a stray mouse-out is the failure mode);
 *   - the `@` picker's "去续投" focus signal reveals AND focuses on its rising
 *     edge, so the human arrives ready to type;
 *   - a send rides `sendGroupText(chat, text, [], referenceRowId)` — the level-1
 *     branch — where the id is the freshest round's reference-row message_id
 *     (same value the pause card's continue button uses), not the block ref.
 */
describe("ChatExecutionBlock in-place reply", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    store.chat = { agentStates: {} };
    vi.restoreAllMocks();
  });

  it("reveals on hover and refuses to hide while a draft holds", () => {
    renderBlock({ [REF1]: entryOf({}) });
    const box = screen.getByRole("textbox") as HTMLTextAreaElement;
    const shell = box.parentElement as HTMLElement;
    const hoverZone = shell.parentElement as HTMLElement;
    // Collapsed at rest: transparent and non-interactive.
    expect(shell.className).toContain("opacity-0");
    fireEvent.mouseEnter(hoverZone);
    expect(shell.className).toContain("opacity-100");
    // Type, then the pointer leaves — the draft pins it open.
    fireEvent.change(box, { target: { value: "keep typing" } });
    fireEvent.mouseLeave(hoverZone);
    expect(shell.className).toContain("opacity-100");
  });

  it("the picker's focus signal reveals and focuses the box", async () => {
    renderBlock({ [REF1]: entryOf({}) }, { focusInput: true });
    const box = screen.getByRole("textbox");
    await waitFor(() => expect(document.activeElement).toBe(box));
    expect((box.parentElement as HTMLElement).className).toContain(
      "opacity-100",
    );
  });

  it("sends an in-place continuation to the freshest round's reference row", async () => {
    const sendGroupText = vi.fn(async () => true);
    vi.spyOn(useChatSessionStore, "getState").mockReturnValue({
      sendGroupText,
    } as unknown as ReturnType<typeof useChatSessionStore.getState>);
    renderBlock({ [REF1]: entryOf({}) });
    const box = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.focus(box);
    fireEvent.change(box, { target: { value: "continue please" } });
    fireEvent.click(screen.getByRole("button", { name: "chatView.block.send" }));
    await waitFor(() =>
      expect(sendGroupText).toHaveBeenCalledWith(
        "c1",
        "continue please",
        [],
        "round:ag|s1|r1",
      ),
    );
    // Cleared after send, box stays put for the next follow-up.
    expect(box.value).toBe("");
  });
});
