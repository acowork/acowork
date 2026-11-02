// @vitest-environment jsdom
/**
 * Card chrome — regression tests for the "no highlight color on card borders /
 * titles" rule.
 *
 * Why this file exists:
 *   User-visible defect: the chat interaction cards had been restyled so the
 *   card OUTLINE and the HEADER TITLE were painted with the brand highlight
 *   (`--color-accent`). The global design language allows the highlight color
 *   on exactly one thing — a BUTTON BACKGROUND (`btn-accent` /
 *   `bg-accent-strong`, globals.css:1649-1670). A card wears the neutral
 *   hairline and its title wears the regular text tier.
 *
 * What this pins:
 *   1. The execution-session card shell (`AskQuestionCard`) is
 *      `border-border-outer` and carries no `--color-accent` anywhere in its
 *      border or header class. Its header uses the shared neutral card-header
 *      recipe (`bg-chat-title` + `text-text`) — the same one CodeBlock uses.
 *   2. The accent BUTTONS inside the cards survive the fix — a card that loses
 *      its accent border must not also lose its primary action.
 *
 * The chat-side card (`InlineInteractionCard`) is gone: ADR-106 D8 moves the
 * round's pending decision into the execution block, whose chrome is pinned in
 * `ChatExecutionBlock.test.tsx`.
 *
 * Why rendered DOM for the card and source scanning for the button guard:
 *   The shell/header class strings are what the browser paints, so reading them
 *   off the rendered element is the honest assertion. The button guard spans two
 *   files whose full render needs the whole chat store harness; the class
 *   literal there is the spec (same trade-off as capsule.test.tsx).
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { AskQuestionCard } from "./AskQuestionCard";

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

describe("interaction card chrome", () => {
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
    for (const file of ["AskQuestionCard.tsx", "ChatExecutionBlock.tsx"]) {
      const src = readFileSync(resolve(__dirname, file), "utf8");
      expect(src, `${file} must keep its btn-accent primary action`).toContain("btn-accent");
    }
  });
});
