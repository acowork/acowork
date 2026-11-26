/**
 * `transcriptHeight` — the estimate a virtualized transcript is made of.
 *
 * The defect these pins exist for: the group Chat was reported as "my normal
 * scrolling is unstable, two or three wheel gestures and the scrollbar is in
 * the middle", while the 1:1 Chat of the same code was fine. A window holds
 * ~40 items and at most a dozen are ever measured, so the pane's total height
 * is mostly ESTIMATES — and a group window is mostly run blocks, which the old
 * `64 + 34 * min(rounds, 3)` under-estimated by 2-5x while also inventing a
 * per-round line the collapsed block never draws. Every row that finally
 * mounted then moved the scroll offset by the whole error.
 */
import { describe, expect, it } from "vitest";
import { estimateRunItemHeight } from "./transcriptHeight";

/** The sum of the chrome the block always paints, with no answer and no card:
 *  padding 8 + header 52 + card border 2 + banner 32 + body padding 20. */
const CHROME = 114;
/** Ten folded lines of chat prose (the `chatBubbleFold` ceiling). */
const CLAMPED_ANSWER = 220;
/** The fold toggle's row, present only when the answer reaches the clamp. */
const TOGGLE = 22;

describe("estimateRunItemHeight", () => {
  it("assumes the ten-line fold when the answer is not known yet", () => {
    expect(estimateRunItemHeight()).toBe(CHROME + CLAMPED_ANSWER + TOGGLE);
  });

  it("clamps a long answer and keeps the fold toggle", () => {
    const long = "x".repeat(2000);
    expect(estimateRunItemHeight({ answerBody: long })).toBe(
      CHROME + CLAMPED_ANSWER + TOGGLE,
    );
  });

  it("drops the toggle for an answer that does not reach the fold", () => {
    // One line of prose: below the ten-line ceiling, so no toggle is drawn.
    expect(estimateRunItemHeight({ answerBody: "done" })).toBe(CHROME + 22);
  });

  it("treats an empty body as unknown, not as a one-line answer", () => {
    // `latestAnswerOf` answers "" for a run that has not answered (or not been
    // read). Sizing that at one line is the under-estimate this module exists to
    // remove, so "" means "unknown" — the same height as omitting the body.
    expect(estimateRunItemHeight({ answerBody: "" })).toBe(
      estimateRunItemHeight(),
    );
  });

  it("never shrinks as the answer grows, and saturates at the ceiling", () => {
    const heights = [1, 70, 140, 700, 1400, 7000].map((n) =>
      estimateRunItemHeight({ answerBody: "y".repeat(n) }),
    );
    for (let i = 1; i < heights.length; i += 1) {
      expect(heights[i]).toBeGreaterThanOrEqual(heights[i - 1]);
    }
    expect(heights.at(-1)).toBe(CHROME + CLAMPED_ANSWER + TOGGLE);
  });

  it("does not take the round count: a collapsed block draws no per-round line", () => {
    // The rounds live in a panel that only mounts when the reader expands the
    // block, so the number of rounds cannot change the collapsed height. This is
    // the shape half of the old defect (the magnitude is the test above).
    const one = estimateRunItemHeight({ answerBody: "ok" });
    const many = estimateRunItemHeight({ answerBody: "ok" });
    expect(many).toBe(one);
  });

  it("adds the router's question when it is hung under the block", () => {
    expect(estimateRunItemHeight({ answerBody: "ok", pendingAsk: true })).toBe(
      CHROME + 22 + 88,
    );
  });

  it("counts the agent header unless the surface folds it", () => {
    expect(estimateRunItemHeight({ answerBody: "ok", showHeader: false })).toBe(
      CHROME - 52 + 22,
    );
  });

  it("is a realistic size for a run, not the old 64-166px fiction", () => {
    // A pinned floor rather than a magic number: if a future edit shrinks the
    // estimate below a real block's own chrome, the group transcript's total
    // height goes back to being wrong from the first screen.
    expect(estimateRunItemHeight()).toBeGreaterThan(300);
  });
});