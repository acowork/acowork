/**
 * Chat card chrome — regression tests for the "no highlight color on card
 * borders / titles" rule.
 *
 * Why this file exists:
 *   User-visible defect: the chat interaction cards had been restyled so the
 *   card OUTLINE and the HEADER TITLE were painted with the brand highlight
 *   (`--color-accent`). The global design language allows the highlight color
 *   on exactly one thing — a BUTTON BACKGROUND (`btn-accent` /
 *   `bg-accent-strong`, globals.css:1649-1670). A card wears the neutral
 *   hairline (`border-border-outer`) and its title wears the regular text
 *   tier (`text-text`). The drift also broke the cards against their own
 *   family: the receipt state in the same component had already used the
 *   neutral recipe (that receipt is since gone — an answered card paints
 *   nothing and the answer bubble quotes the question; see ReplyQuote).
 *
 * What this pins:
 *   1. Every OPEN / READ-ONLY interaction card shell is `border-border-outer`
 *      and carries no `--color-accent` anywhere in its border or header class.
 *   2. The header bar uses the shared neutral card-header recipe
 *      (`bg-chat-title` + `text-text`) — the same one CodeBlock uses, so the
 *      fix is an alignment, not a new style.
 *   3. The approval / pause kinds share the same neutral chrome as
 *      ask_question — kinds are told apart by icon + title, not color.
 *      (The former emerald / amber semantic borders were unified away by
 *      explicit user decision.)
 *   4. The accent BUTTONS inside the cards survive the fix — a card that
 *      loses its accent border must not also lose its primary action.
 *   5. Two-button action cards (approval / pause) wear a narrow `max-w-sm`
 *      clamp so long tool text wraps instead of stretching the card.
 *
 * Why rendered DOM for the cards and source scanning for the button guard:
 *   The shell/header class strings are what the browser paints, so reading
 *   them off the rendered element is the honest assertion. The button guard
 *   spans two files whose full render needs the whole chat store harness;
 *   the class literal there is the spec (same trade-off as capsule.test.tsx).
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { InlineInteractionCard } from "./InlineInteractionCard";
import { AskQuestionCard } from "./AskQuestionCard";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

// AskQuestionCard reads the runtime heartbeat for its countdown; an empty
// agent-state map means "no progress yet", which is the state we style-test.
vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector({ agentStates: {} }),
    { getState: () => ({}) },
  ),
}));

const __dirname = dirname(fileURLToPath(import.meta.url));

type Interaction = NonNullable<ChatMessageRow["meta"]>["interaction"];

function cardRow(messageId: string, interaction: Interaction): ChatMessageRow {
  return {
    seq: 1,
    ts: 1,
    from: "ag",
    kind: "interaction_request",
    body: "",
    to: "u",
    meta: { message_id: messageId, interaction },
  };
}

const ASK = {
  kind: "ask_question",
  request_id: "q1",
  question: "Pick one",
  options: ["A", "B"],
};

/** The card shell is the `overflow-hidden rounded-md border …` wrapper; the
 *  header bar is its first child. */
function shellOf(container: HTMLElement): HTMLElement {
  const shell = container.querySelector("div[class*='overflow-hidden'][class*='border']");
  if (!shell) throw new Error("card shell not found");
  return shell as HTMLElement;
}

function headerOf(shell: HTMLElement): HTMLElement {
  const header = shell.firstElementChild as HTMLElement;
  expect(header?.className).toContain("border-b");
  return header;
}

describe("chat interaction card chrome", () => {
  it("ask_question open state: neutral border, no highlight color on the shell", () => {
    const { container } = render(
      <InlineInteractionCard
        row={cardRow("card1", ASK)}
        closed={new Set()}
        answered={new Set()}
        onAnswer={vi.fn()}
      />,
    );
    const shell = shellOf(container);
    expect(shell.className).toContain("border-border-outer");
    expect(shell.className).not.toContain("--color-accent");
  });

  it("ask_question open state: header wears the neutral card-header recipe", () => {
    const { container } = render(
      <InlineInteractionCard
        row={cardRow("card1", ASK)}
        closed={new Set()}
        answered={new Set()}
        onAnswer={vi.fn()}
      />,
    );
    const header = headerOf(shellOf(container));
    expect(header.className).toContain("bg-chat-title");
    expect(header.className).toContain("text-text");
    expect(header.className).not.toContain("--color-accent");
  });

  it("read-only variant shares the same neutral chrome (one cfg, both entries)", () => {
    const { container } = render(
      <InlineInteractionCard
        row={cardRow("card1", ASK)}
        closed={new Set()}
        answered={new Set()}
        onAnswer={vi.fn()}
        readOnly
      />,
    );
    const shell = shellOf(container);
    expect(shell.className).toContain("border-border-outer");
    expect(shell.className).not.toContain("--color-accent");
    expect(headerOf(shell).className).not.toContain("--color-accent");
  });

  it("paints no shell at all once the card is answered", () => {
    const { container } = render(
      <InlineInteractionCard
        row={cardRow("card1", ASK)}
        closed={new Set(["card1"])}
        answered={new Set(["card1"])}
        onAnswer={vi.fn()}
      />,
    );
    // The answered receipt no longer exists, so it cannot drift back to an
    // accent border: there is nothing to paint. The question it used to repeat
    // now rides on the answer bubble as a quote line (ReplyQuote).
    expect(container.firstChild).toBeNull();
  });

  it("approval / pause wear the same neutral chrome as ask (icon+title tell them apart)", () => {
    const { container: approveBox } = render(
      <InlineInteractionCard
        row={cardRow("c2", { kind: "approval", request_id: "a1", question: "Approve?" })}
        closed={new Set()}
        answered={new Set()}
        onAnswer={vi.fn()}
      />,
    );
    expect(shellOf(approveBox).className).toContain("border-border-outer");
    expect(shellOf(approveBox).className).not.toMatch(/emerald|amber|--color-accent/);
    // Narrow clamp: long tool text wraps instead of stretching the card.
    expect((approveBox.firstChild as HTMLElement).className).toContain("max-w-sm");

    const { container: pauseBox } = render(
      <InlineInteractionCard
        row={cardRow("c3", { kind: "pause", request_id: "p1", question: "Paused" })}
        closed={new Set()}
        answered={new Set()}
        onAnswer={vi.fn()}
      />,
    );
    expect(shellOf(pauseBox).className).toContain("border-border-outer");
    expect(shellOf(pauseBox).className).not.toMatch(/emerald|amber|--color-accent/);
    expect((pauseBox.firstChild as HTMLElement).className).toContain("max-w-sm");
  });

  it("AskQuestionCard (execute-session surface): neutral border and header", () => {
    const { container } = render(
      <AskQuestionCard
        event={{
          request_id: "q1",
          question: "Pick one",
          options: [{ label: "A" }, { label: "B" }],
        } as never}
        agentId="ag"
        sessionId="s1"
        onAnswer={vi.fn()}
      />,
    );
    const shell = shellOf(container);
    expect(shell.className).toContain("border-border-outer");
    expect(shell.className).not.toContain("--color-accent");
    const header = headerOf(shell);
    expect(header.className).toContain("bg-chat-title");
    expect(header.className).toContain("text-text");
    expect(header.className).not.toContain("--color-accent");
  });

  it("the fix does not strip the accent BUTTONS inside the cards", () => {
    for (const file of ["InlineInteractionCard.tsx", "AskQuestionCard.tsx"]) {
      const src = readFileSync(resolve(__dirname, file), "utf8");
      expect(src, `${file} must keep its btn-accent primary action`).toContain("btn-accent");
    }
  });
});
