// @vitest-environment jsdom
/**
 * ReplyQuote — the one line above an answer bubble naming what it answers.
 *
 * It replaced the answered receipt card, so it inherits that card's job (keep
 * the question readable next to the answer) and none of its weight. Two rules
 * this pins, both of which a later edit can break in isolation:
 *
 *   - It stays ONE line whatever the question holds: a question is arbitrary
 *     model output, and a wrapping quote strip would push the answer down and
 *     read as a second card.
 *   - Its chrome is neutral. The line sits directly above the own bubble, which
 *     is filled with the highlight color (`--color-chat-user` = accent-strong),
 *     so an accent border or label here would stack the highlight on the
 *     highlight — the exact over-reach the card chrome rule forbids.
 */
import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { ReplyQuote, truncateQuote } from "./ReplyQuote";

const stripOf = (box: HTMLElement) => {
  const el = box.querySelector("span");
  if (!el) throw new Error("no quote strip rendered");
  return el;
};

describe("truncateQuote", () => {
  it("collapses newlines and runs of spaces into one line", () => {
    expect(truncateQuote("  pick\n a   schema  ")).toBe("pick a schema");
  });

  it("bounds a long question with an ellipsis", () => {
    const out = truncateQuote("x".repeat(200));
    expect(out.length).toBe(61); // 60 chars + the ellipsis
    expect(out.endsWith("…")).toBe(true);
  });

  it("returns empty for blank input so the caller paints nothing", () => {
    expect(truncateQuote(undefined)).toBe("");
    expect(truncateQuote("   \n ")).toBe("");
  });
});

describe("ReplyQuote", () => {
  it("paints nothing when there is no question to quote", () => {
    const { container } = render(<ReplyQuote quote="   " />);
    expect(container.firstChild).toBeNull();
    expect(render(<ReplyQuote quote={undefined} />).container.firstChild).toBeNull();
  });

  it("shows the collapsed question and keeps the full text on hover", () => {
    const long = `first line\n${"y".repeat(200)}`;
    const { container } = render(<ReplyQuote quote={long} />);
    const strip = stripOf(container);
    // "first line " is 11 chars, so the 60-char budget leaves room for 49 y's.
    expect(strip.textContent).toBe(`first line ${"y".repeat(49)}…`);
    // The untruncated text stays reachable — a bounded line must not be a lie.
    expect(container.querySelector("div")?.getAttribute("title")).toBe(long);
  });

  it("wears neutral chrome: a divider hairline and a tertiary label", () => {
    const { container } = render(<ReplyQuote quote="Which schema?" />);
    const cls = stripOf(container).className;
    expect(cls).toContain("border-border-divider");
    expect(cls).toContain("text-text-tertiary");
    expect(cls).not.toContain("--color-accent");
    expect(cls).not.toContain("border-border-outer");
  });

  it("aligns to the bubble it belongs above", () => {
    const own = render(<ReplyQuote quote="q" align="end" />);
    expect(own.container.querySelector("div")?.className).toContain("justify-end");
    const other = render(<ReplyQuote quote="q" align="start" />);
    expect(other.container.querySelector("div")?.className).toContain("justify-start");
  });
});
