// @vitest-environment jsdom
/**
 * Markdown inside an interaction card's question.
 *
 * The reported symptom: a card whose question carried markdown painted the
 * syntax itself — "Approve `bash`?" with the backticks, and a link as
 * "[file.ts:12](path#L12)". The question is model output, so markdown in it is
 * normal, and the transcript right next to it renders that markdown: the card
 * was the only chat surface that did not. The answered receipt showed the same
 * raw text, which is what made it visible in the screenshot that started this.
 *
 * Both card surfaces are pinned because both had the same bare text node:
 *   - InlineInteractionCard — the durable chat card, in its OPEN and READ-ONLY
 *     variants (the closed note stays plain text on purpose: it is a one-line
 *     status note, and a `title` attribute cannot hold markup anyway);
 *   - AskQuestionCard — the execution-session card.
 *
 * The assertions check for the ABSENCE of raw syntax as well as the presence of
 * rendered elements: a regression that re-renders the question as text would
 * otherwise still pass on "a <code> exists somewhere in the tree".
 */
import { render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { InlineInteractionCard } from "./InlineInteractionCard";
import { AskQuestionCard } from "./AskQuestionCard";
import type { ChatMessageRow } from "../../stores/chatSessionStore";
import type { AskQuestionEvent } from "../../lib/types";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../editor/markdownLinkResolver", () => ({
  handleChatMarkdownLinkClick: vi.fn(),
  markdownUrlTransform: (url: string) => url,
}));
vi.mock("./CodeBlock", () => ({
  CodeBlock: ({ code }: { language: string; code: string }) => <code>{code}</code>,
}));
vi.mock("../../stores/chatStore", () => ({
  useChatStore: (selector: (state: unknown) => unknown) => selector({ agentStates: {} }),
}));

/** A question with the two constructs that were showing up raw: inline code and
 *  a workspace link. */
const MD_QUESTION = "Approve `bash`? see [the plan](docs/plan.md)";

function cardRow(question: string): ChatMessageRow {
  return {
    seq: 1,
    ts: 1,
    from: "ag",
    kind: "interaction_request",
    body: question,
    to: "u",
    meta: {
      message_id: "card1",
      interaction: { kind: "ask_question", request_id: "q1", question },
    },
  };
}

function assertRenderedMarkdown(container: HTMLElement) {
  const text = container.textContent ?? "";
  // The syntax itself must be gone, not merely accompanied by rendered nodes.
  expect(text).not.toContain("`bash`");
  expect(text).not.toContain("[the plan](");
  expect(text).toContain("Approve ");
  expect(text).toContain("the plan");
  // Inline code and a real link element take their place.
  expect(container.querySelector("code")?.textContent).toBe("bash");
  const link = container.querySelector("a");
  expect(link?.textContent).toBe("the plan");
  expect(link?.getAttribute("href")).toBe("docs/plan.md");
}

describe("interaction card question markdown", () => {
  it("renders the open chat card's question as markdown", () => {
    const { container } = render(
      <InlineInteractionCard
        row={cardRow(MD_QUESTION)}
        closed={new Set()}
        answered={new Set()}
        onAnswer={vi.fn()}
      />,
    );
    assertRenderedMarkdown(container);
  });

  it("renders the read-only chat card's question as markdown", () => {
    const { container } = render(
      <InlineInteractionCard
        row={cardRow(MD_QUESTION)}
        closed={new Set()}
        answered={new Set()}
        onAnswer={vi.fn()}
        readOnly
      />,
    );
    assertRenderedMarkdown(container);
  });

  it("renders the execution-session card's question as markdown", () => {
    const event: AskQuestionEvent = {
      request_id: "q1",
      question: MD_QUESTION,
      options: [],
    } as AskQuestionEvent;
    const { container } = render(
      <AskQuestionCard event={event} agentId="ag" sessionId="s1" onAnswer={vi.fn()} />,
    );
    assertRenderedMarkdown(container);
  });

  it("keeps the closed-without-answer note plain (a title cannot hold markup)", () => {
    const { container } = render(
      <InlineInteractionCard
        row={cardRow(MD_QUESTION)}
        closed={new Set(["card1"])}
        answered={new Set()}
        onAnswer={vi.fn()}
      />,
    );
    // One line, no block elements: the note is a status line, not a body.
    expect(container.querySelector("p")).toBeNull();
    expect(container.querySelector("a")).toBeNull();
    // The full text stays reachable on hover, backticks and all.
    expect(container.querySelector("div")?.getAttribute("title")).toBe(MD_QUESTION);
  });
});
