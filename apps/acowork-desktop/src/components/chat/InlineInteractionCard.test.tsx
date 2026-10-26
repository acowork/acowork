import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { InlineInteractionCard } from "./InlineInteractionCard";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

// `t` echoes the key (plus the interpolated question when a call passes one)
// so assertions read against the i18n vocabulary directly.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, string>) =>
      params && params.question !== undefined ? `${key}«${params.question}»` : key,
  }),
}));

type Interaction = NonNullable<ChatMessageRow["meta"]>["interaction"];

function cardRow(
  messageId: string,
  interaction: Interaction,
  extra: Partial<ChatMessageRow["meta"]> = {},
): ChatMessageRow {
  return {
    seq: 1,
    ts: 1,
    from: "ag",
    kind: "interaction_request",
    body: "",
    to: "u",
    meta: { message_id: messageId, interaction, ...extra },
  };
}

const askOpts = { kind: "ask_question", request_id: "q1", question: "Pick one", options: ["A", "B"] };

describe("InlineInteractionCard", () => {
  it("renders a header bar with the ask title", () => {
    const row = cardRow("card1", askOpts);
    render(
      <InlineInteractionCard row={row} closed={new Set()} answered={new Set()} onAnswer={vi.fn()} />,
    );
    expect(screen.getByText("chatView.cardAsk")).toBeTruthy();
  });

  it("list mode: selecting an option then submit fires onAnswer immediately", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", askOpts);
    render(
      <InlineInteractionCard row={row} closed={new Set()} answered={new Set()} onAnswer={onAnswer} />,
    );
    // Click option "A" to select it
    fireEvent.click(screen.getByText("A"));
    // Click submit
    fireEvent.click(screen.getByText("askQuestionCard.submit"));
    expect(onAnswer).toHaveBeenCalledWith(row, "A", true);
  });

  it("list mode: submit is disabled before selection", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", askOpts);
    render(
      <InlineInteractionCard row={row} closed={new Set()} answered={new Set()} onAnswer={onAnswer} />,
    );
    const submitBtn = screen.getByText("askQuestionCard.submit");
    expect(submitBtn.closest("button")?.disabled).toBe(true);
  });

  it("free-answer button opens composer (non-immediate)", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", askOpts);
    render(
      <InlineInteractionCard row={row} closed={new Set()} answered={new Set()} onAnswer={onAnswer} />,
    );
    fireEvent.click(screen.getByText("chatView.freeAnswer"));
    expect(onAnswer).toHaveBeenCalledWith(row, "", false);
  });

  it("approval: shows approve and reject buttons", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", { kind: "approval", request_id: "a1", question: "Approve?" });
    render(
      <InlineInteractionCard row={row} closed={new Set()} answered={new Set()} onAnswer={onAnswer} />,
    );
    expect(screen.getByText("chatView.cardApproval")).toBeTruthy();
    fireEvent.click(screen.getByText("chatView.cardApprove"));
    // The sent text is the Chinese "批准" which is_affirmative recognises
    expect(onAnswer).toHaveBeenCalledWith(row, "\u6279\u51c6", true);
  });

  it("approval: reject sends reject text", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", { kind: "approval", request_id: "a1", question: "Approve?" });
    render(
      <InlineInteractionCard row={row} closed={new Set()} answered={new Set()} onAnswer={onAnswer} />,
    );
    fireEvent.click(screen.getByText("chatView.cardReject"));
    expect(onAnswer).toHaveBeenCalledWith(row, "\u62d2\u7edd", true);
  });

  it("pause: shows continue and stop buttons", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", { kind: "pause", request_id: "p1", question: "Step done" });
    render(
      <InlineInteractionCard row={row} closed={new Set()} answered={new Set()} onAnswer={onAnswer} />,
    );
    expect(screen.getByText("chatView.cardPause")).toBeTruthy();
    fireEvent.click(screen.getByText("chatView.cardContinue"));
    expect(onAnswer).toHaveBeenCalledWith(row, "\u7ee7\u7eed", true);
  });

  it("no options + no actions = single answer button", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", { kind: "ask_question", request_id: "q2", question: "How?" });
    render(
      <InlineInteractionCard row={row} closed={new Set()} answered={new Set()} onAnswer={onAnswer} />,
    );
    fireEvent.click(screen.getByText("chatView.answer"));
    expect(onAnswer).toHaveBeenCalledWith(row, "", false);
  });

  it("paints nothing once the card is answered", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", askOpts);
    const { container } = render(
      <InlineInteractionCard
        row={row}
        closed={new Set(["card1"])}
        answered={new Set(["card1"])}
        onAnswer={onAnswer}
      />,
    );
    // The receipt card is gone: it repeated the question and said nothing about
    // who answered or what they said. The answer bubble's quote line carries the
    // link now (see ReplyQuote + AgentChatView / GroupChatPanel).
    expect(container.firstChild).toBeNull();
    expect(screen.queryByText(/chatView.cardAnswered/)).toBeNull();
    expect(screen.queryByText("chatView.freeAnswer")).toBeNull();
    expect(onAnswer).not.toHaveBeenCalled();
  });

  it("leaves a quiet one-line note when a card closed with no answer", () => {
    const row = cardRow("card1", { ...askOpts, question: "Deploy now?" });
    const { container } = render(
      <InlineInteractionCard
        row={row}
        closed={new Set(["card1"])}
        answered={new Set()}
        onAnswer={vi.fn()}
      />,
    );
    // The fact that a decision was requested and never got one exists nowhere
    // else in the transcript, so it must stay visible — but as a line, not a
    // card shell: no border, no header band, no fill.
    // Exact string, not a regex: the quoted question contains `?`, which a
    // RegExp would read as a quantifier.
    expect(screen.getByText("chatView.cardUnanswered«Deploy now?»")).toBeTruthy();
    const line = container.querySelector("div");
    expect(line?.className).not.toContain("border");
    expect(line?.className).not.toContain("bg-");
    expect(line?.className).toContain("text-text-tertiary");
    // And it stays inert: there is nothing left to click.
    expect(container.querySelector("button")).toBeNull();
  });

  it("renders an answerable question with no affordance in read-only mode", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", askOpts);
    render(
      <InlineInteractionCard
        row={row}
        closed={new Set()}
        answered={new Set()}
        askerName="Agent A"
        readOnly
        onAnswer={onAnswer}
      />,
    );
    expect(screen.getByText("Pick one")).toBeTruthy();
    expect(screen.getByText("Agent A")).toBeTruthy();
    expect(screen.queryByText("chatView.freeAnswer")).toBeNull();
    expect(onAnswer).not.toHaveBeenCalled();
  });
});
