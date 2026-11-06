/**
 * `runBlockText.ts` — the shape of a block's copy export.
 *
 * Locked behaviours:
 * - every round is exported, in the session's own order (newest last), so
 *   "copy this run" never silently drops the rounds behind "show earlier";
 * - a round whose content could not be read exports a HOLE, not an invented
 *   answer (ADR-106 D3/D5);
 * - an answer body is pasted verbatim, so its Markdown survives the paste;
 * - a question the run never received leaves no dangling prefix.
 */
import { describe, expect, it } from "vitest";
import { runBlockToText, type RunBlockTextLabels } from "./runBlockText";

const labels: RunBlockTextLabels = {
  round: (n) => `Round ${n}`,
  questionPrefix: "Q: ",
  unreadable: "(this round could not be read)",
};

describe("runBlockToText", () => {
  it("exports the name and every round in order, newest last", () => {
    const text = runBlockToText({
      name: "Fix the login redirect",
      labels,
      rounds: [
        { index: 1, question: "why is it looping?", answer: "cookie domain" },
        { index: 2, question: "and now?", answer: "fixed" },
      ],
    });
    expect(text).toBe(
      [
        "# Fix the login redirect",
        "",
        "## Round 1",
        "",
        "Q: why is it looping?",
        "",
        "cookie domain",
        "",
        "## Round 2",
        "",
        "Q: and now?",
        "",
        "fixed",
        "",
      ].join("\n"),
    );
  });

  it("keeps an answer's Markdown verbatim", () => {
    const answer = "Steps:\n\n```ts\nconst a = 1;\n```\n\n- one\n- two";
    const text = runBlockToText({
      name: "run",
      labels,
      rounds: [{ index: 1, question: "", answer }],
    });
    expect(text).toContain(answer);
  });

  it("states an unreadable round instead of inventing one", () => {
    const text = runBlockToText({
      name: "run",
      labels,
      rounds: [{ index: 3, question: "q", answer: "", unreadable: true }],
    });
    expect(text).toContain("## Round 3");
    expect(text).toContain("(this round could not be read)");
    // The hole replaces the body; nothing else is written under the heading.
    expect(text.trimEnd().endsWith("(this round could not be read)")).toBe(true);
  });

  it("leaves no dangling prefix for a round that received no question", () => {
    const text = runBlockToText({
      name: "run",
      labels,
      rounds: [{ index: 1, question: "   ", answer: "did it anyway" }],
    });
    expect(text).not.toContain("Q:");
    expect(text).toContain("did it anyway");
  });

  it("still names a session it cannot render any round of", () => {
    const text = runBlockToText({ name: "Empty run", labels, rounds: [] });
    expect(text).toBe("# Empty run\n");
  });
});
