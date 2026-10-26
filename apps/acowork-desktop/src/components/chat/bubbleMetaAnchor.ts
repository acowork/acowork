/**
 * Which horizontal edge a bubble's meta block hugs.
 *
 * A bubble's meta block is everything that belongs to one utterance but is
 * not the utterance itself: the §5.5.4 quote line naming what an answer
 * answers, the "view run" drill, and the hover timestamp. The rule is the IM
 * convention and it is the fix for a reported defect:
 *
 *   **the meta block hugs the bubble's OWN outer edge** — `start` for a peer's
 *   left-aligned bubble, `end` for the human's right-aligned one.
 *
 * What it must never do is hug the ROW's start edge. With a shrink-wrapped
 * bubble that put the human's own timestamp at the bubble's LEFT edge, so one
 * and the same fact surfaced near the row's middle for a short message
 * ("批准") and at the far left for a long one — "sometimes in the middle,
 * sometimes on the left, is there a rule?". Worse, the surface that already
 * did it right made the drift visible: the group panel hugged the outer edge
 * while the 1:1 chat and the execution surface did not, so the SAME message
 * showed its time under its right edge in a group and under its left edge in
 * a 1:1.
 *
 * Extracted so the rule has exactly one home — the class string had been
 * copied per surface, which is how four render paths ended up with two
 * answers — and so it can be unit-tested without mounting React, the same
 * shape `avatarAnchor.ts` uses for its own layout rule. jsdom performs no
 * layout, so the rendered test guards the DOM shape (which column carries
 * which edge), not painted geometry.
 */

/** The bubble's side, from the viewer's point of view: `own` = the human who
 *  is reading (right-aligned, `bg-chat-user`); `peer` = everyone else. */
export type BubbleSide = "own" | "peer";

/** Cross-axis alignment for a bubble column / meta row (a Tailwind
 *  `align-items` class). */
export function bubbleMetaAnchor(side: BubbleSide): "items-end" | "items-start" {
  // `own` → end. The human's message column is right-aligned on its row, so
  // its meta block must hug the right edge; anything else strands the
  // timestamp under the bubble's left edge, which reads as "placed at random".
  return side === "own" ? "items-end" : "items-start";
}