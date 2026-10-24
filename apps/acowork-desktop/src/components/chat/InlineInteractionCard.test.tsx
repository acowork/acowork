import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { InlineInteractionCard } from "./InlineInteractionCard";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

// `t` echoes the key so assertions read against the i18n vocabulary directly.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
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

const opts = { kind: "ask_question", request_id: "q1", question: "Pick one", options: ["A", "B"] };

describe("InlineInteractionCard", () => {
  it("answers an option in place through the immediate channel", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", opts);
    render(
      <InlineInteractionCard row={row} closed={new Set()} answered={new Set()} onAnswer={onAnswer} />,
    );
    expect(screen.getByText("Pick one")).toBeTruthy();
    fireEvent.click(screen.getByText("A"));
    expect(onAnswer).toHaveBeenCalledWith(row, "A", true);
  });

  it("routes a free-text answer through the composer (non-immediate)", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", opts);
    render(
      <InlineInteractionCard row={row} closed={new Set()} answered={new Set()} onAnswer={onAnswer} />,
    );
    fireEvent.click(screen.getByText("chatView.freeAnswer"));
    expect(onAnswer).toHaveBeenCalledWith(row, "", false);
  });

  it("shows a single answer button when the request has no options", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", { kind: "approval", request_id: "a1", question: "Approve?" });
    render(
      <InlineInteractionCard row={row} closed={new Set()} answered={new Set()} onAnswer={onAnswer} />,
    );
    fireEvent.click(screen.getByText("chatView.answer"));
    expect(onAnswer).toHaveBeenCalledWith(row, "", false);
  });

  it("collapses an answered card into a read-only receipt", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", opts);
    render(
      <InlineInteractionCard
        row={row}
        closed={new Set(["card1"])}
        answered={new Set(["card1"])}
        onAnswer={onAnswer}
      />,
    );
    expect(screen.getByText(/chatView.cardAnswered/)).toBeTruthy();
    expect(screen.queryByText("chatView.freeAnswer")).toBeNull();
    expect(onAnswer).not.toHaveBeenCalled();
  });

  it("collapses a closed-but-unanswered card into a closed receipt", () => {
    const row = cardRow("card1", opts);
    render(
      <InlineInteractionCard
        row={row}
        closed={new Set(["card1"])}
        answered={new Set()}
        onAnswer={vi.fn()}
      />,
    );
    expect(screen.getByText(/chatView.cardClosed/)).toBeTruthy();
  });

  it("renders an answerable question with no affordance in read-only mode", () => {
    const onAnswer = vi.fn();
    const row = cardRow("card1", opts);
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
    expect(screen.queryByText("A")).toBeNull();
    expect(onAnswer).not.toHaveBeenCalled();
  });
});
