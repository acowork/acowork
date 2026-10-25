// @vitest-environment jsdom
/**
 * ChatMarkdown — the shared chat markdown renderer.
 *
 * This component exists because the shell + overrides were copied into three
 * surfaces (`AgentChatView`, `GroupChatPanel`, `MessageBubble`) and had already
 * drifted: the group copy dropped `dedent` on fenced code and painted links with
 * a hard-coded accent class instead of the house `.prose a`. The markdown-table
 * defect was the same failure mode one level up — a copy that lost the `prose`
 * scope, so a table parsed but painted as plain text.
 *
 * So the pins here are the ones that keep the copies from diverging again:
 *   1. the shell carries the `prose` scope class (globals.css table rules hang
 *      off it — this is the exact thing that went missing in the group defect);
 *   2. a GFM table is wrapped in `.prose-table-scroll` (a wide table must
 *      scroll, not widen the bubble);
 *   3. a fenced block becomes a `CodeBlock` and arrives DEDENTED (the behaviour
 *      the group copy had lost);
 *   4. a link routes through the chat link resolver and never to the webview
 *      default (which would navigate the chat away);
 *   5. `components` merges over the shared set rather than replacing it, so a
 *      surface can add block typography without silently dropping link routing.
 */
import { fireEvent, render } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { CHAT_MARKDOWN_SHELL_CLASS, ChatMarkdown } from "./ChatMarkdown";
import { handleChatMarkdownLinkClick } from "../editor/markdownLinkResolver";

vi.mock("../editor/markdownLinkResolver", () => ({
  handleChatMarkdownLinkClick: vi.fn(),
  markdownUrlTransform: (url: string) => url,
}));

vi.mock("./CodeBlock", () => ({
  CodeBlock: ({ language, code }: { language: string; code: string }) => (
    <div data-testid="code-block" data-language={language}>
      {code}
    </div>
  ),
}));

const shellOf = (box: HTMLElement) => box.firstElementChild as HTMLElement;

describe("ChatMarkdown", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("wraps content in the prose scope the table rules hang under", () => {
    const { container } = render(<ChatMarkdown content={"| a | b |\n| - | - |\n| 1 | 2 |"} />);
    const shell = shellOf(container);
    // Compared against a literal, NOT against CHAT_MARKDOWN_SHELL_CLASS: the
    // component builds its own className from that constant, so comparing the
    // two is tautological — it stays green even if the constant loses every
    // class in it.
    expect(shell.className).toBe(
      "prose prose-sm prose-zinc max-w-none prose-h1:text-lg prose-h2:text-base " +
        "prose-h3:text-sm prose-h4:text-sm prose-headings:font-semibold select-text " +
        "break-words [&_th]:bg-chat-title [&_td]:bg-chat-body [&_tbody_tr]:!bg-transparent",
    );
    // `prose` must stand ALONE: the globals.css selectors are `.prose`, and a
    // bare toContain("prose") is also satisfied by "prose-sm".
    expect(shell.className.split(/\s+/)).toContain("prose");
    // The scroll wrapper every desktop markdown surface uses.
    expect(shell.querySelector(".prose-table-scroll table")).toBeTruthy();
  });

  it("renders a fenced block as a CodeBlock with the common indent removed", () => {
    const md = "```ts\n    const a = 1;\n    const b = 2;\n```";
    const { getByTestId } = render(<ChatMarkdown content={md} />);
    const block = getByTestId("code-block");
    expect(block.getAttribute("data-language")).toBe("ts");
    // Dedent: 4-space common indent stripped, relative structure kept.
    expect(block.textContent).toBe("const a = 1;\nconst b = 2;");
  });

  it("routes a link through the chat resolver instead of the webview default", () => {
    const { container } = render(<ChatMarkdown content={"see [the plan](docs/plan.md)"} />);
    const link = container.querySelector("a")!;
    fireEvent.click(link);
    expect(handleChatMarkdownLinkClick).toHaveBeenCalledWith("docs/plan.md");
  });

  it("merges surface components over the shared set", () => {
    // The group surface overrides `p` for block spacing. If the merge replaced
    // the whole map instead of layering, links would silently stop routing.
    const { container } = render(
      <ChatMarkdown
        content={"para\n\n[link](x.md)"}
        components={{
          p: ({ children }) => <p data-testid="custom-p">{children}</p>,
        }}
      />,
    );
    expect(container.querySelector("[data-testid='custom-p']")).toBeTruthy();
    fireEvent.click(container.querySelector("a")!);
    expect(handleChatMarkdownLinkClick).toHaveBeenCalledWith("x.md");
  });

  it("appends caller classes without dropping the prose scope", () => {
    const { container } = render(<ChatMarkdown content="hi" className="text-center" />);
    const cls = shellOf(container).className;
    expect(cls).toContain("text-center");
    expect(cls).toContain("prose");
  });
});
