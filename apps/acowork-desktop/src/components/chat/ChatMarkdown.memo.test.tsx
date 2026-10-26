// @vitest-environment jsdom
/**
 * ChatMarkdown memoization (chat-transcript render-perf plan L1).
 *
 * react-markdown v10 parses synchronously on every render — the library
 * carries no memo of its own. While the chat surfaces rendered every loaded
 * row on every composer keystroke, that meant one full `parse+runSync` per
 * markdown body per keystroke. `ChatMarkdown` is now wrapped in `React.memo`;
 * this test pins the two halves of the contract:
 *
 *   1. identical props through the parent → NO second parse (memo hit);
 *   2. changed content → parse runs again (memo must not swallow updates).
 *
 * The `react-markdown` module is replaced with a counting stub, so the
 * assertion measures parse calls, not DOM luck.
 */
import { render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

const spy = vi.hoisted(() => ({ count: 0 }));

vi.mock("react-markdown", () => ({
  default: ({ children }: { children?: React.ReactNode }) => {
    spy.count += 1;
    return <div data-testid="markdown">{children}</div>;
  },
}));
vi.mock("./CodeBlock", () => ({ CodeBlock: () => <div /> }));
vi.mock("../editor/markdownLinkResolver", () => ({
  handleChatMarkdownLinkClick: vi.fn(),
  markdownUrlTransform: (url: string) => url,
}));

import { ChatMarkdown } from "./ChatMarkdown";

describe("ChatMarkdown memo (L1 render guard)", () => {
  it("does not re-parse when the parent re-renders with identical props", () => {
    spy.count = 0;
    const { rerender } = render(<ChatMarkdown content={"hello **world**"} />);
    expect(spy.count).toBe(1);

    // A host re-render (the composer keystroke path) hands the same content
    // and the same (absent) components reference — the memo must hit.
    rerender(<ChatMarkdown content={"hello **world**"} />);
    rerender(<ChatMarkdown content={"hello **world**"} />);
    expect(spy.count).toBe(1);
  });

  it("re-parses when the content changes (memo must not swallow updates)", () => {
    spy.count = 0;
    const { rerender } = render(<ChatMarkdown content={"first"} />);
    rerender(<ChatMarkdown content={"second"} />);
    expect(spy.count).toBe(2);
  });

  it("holds the hit across a stable module-level components reference", () => {
    // The surface contract: pass a module constant (GroupChatPanel's
    // chatBlockComponents). An inline object literal is a fresh identity per
    // render and legitimately misses — that is the caller's to keep stable.
    const components = { p: ({ children }: { children?: React.ReactNode }) => <p>{children}</p> };
    spy.count = 0;
    const { rerender } = render(<ChatMarkdown content={"a"} components={components} />);
    rerender(<ChatMarkdown content={"a"} components={components} />);
    expect(spy.count).toBe(1);
  });
});
