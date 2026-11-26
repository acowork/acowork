/**
 * transcriptHeight — how tall a CHAT TRANSCRIPT item is, before it is measured.
 *
 * Why this module exists. `VirtualRowList` sizes everything it has not mounted
 * from an estimate, so the estimate is what the pane's total height (and its
 * scrollbar) is made of: with ~40 rows in a window, at most a dozen are ever
 * measured, and every other one contributes its estimate forever. An estimate
 * that is off by 5x does not just place a row wrongly — it makes the scroll
 * thumb a linear map of a lie, and every correction when a row finally mounts
 * moves the reader's scroll offset by that error. That is the difference the
 * reader reported between the two Chat surfaces: a 1:1 window is mostly short
 * bubbles whose estimate is close, a group window is mostly RUN BLOCKS whose
 * estimate was not.
 *
 * What was wrong with the old one (`64 + 34 * min(rounds, 3)`, ≤ 166px): it
 * charged a line per round, but a collapsed run block renders NO per-round
 * line. `ChatExecutionBlock` draws its rounds inside a panel that is only
 * mounted when the reader expands the block (`{expanded && …}`, default closed),
 * so the number of rounds does not change the height at all; what does is the
 * answer under the banner, which is folded at ten lines (`chatBubbleFold`).
 * Both terms were wrong: the magnitude (the real block is ~2-5x taller) and the
 * shape (it does not vary with the round count).
 *
 * So the height below is a sum over the parts `ChatExecutionBlock` actually
 * paints, and every constant names the class it mirrors — the same discipline
 * `blockLayout.ts` keeps for the execution surface. Keep them in step: a
 * padding change in the component that is not mirrored here re-introduces the
 * drift this module exists to remove.
 *
 * What it deliberately does not model:
 * - the reader's own expansion of a block's rounds. That is component-local
 *   state (`ChatExecutionBlock`'s `expanded`), not a property of the row, so it
 *   cannot be estimated here; the shell measures the block when it mounts, and
 *   a height the reader asked for is not an estimate error.
 * - the answer's real markdown (headings, lists, code fences, images). The fold
 *   caps it at ten lines, so the ceiling — not the content — is what bounds the
 *   error, provided the answer is long enough to reach it.
 */

import { AGENT_HEADER_HEIGHT } from "./blockLayout";
import { RUN_ANSWER_COLLAPSED_LINES, collapsedHeightPx } from "./chatBubbleFold";

/** `div.px-4 py-1` on the block's root: 4 + 4. */
const ITEM_PADDING_Y = 8;
/** `mt-1` above the agent header, inside the block. */
const HEADER_MARGIN_TOP = 4;
/** `chatCardShell`: one 1px border top and bottom. */
const CARD_BORDER_Y = 2;
/** `chatCardTitleBarMuted`: `px-3 py-[7px]` (14) + a `text-xs` line box (18). */
const TITLE_BAR_H = 32;
/** `chatCardBody`: `py-2.5` = 10 + 10. */
const ANSWER_BODY_PADDING_Y = 20;
/** The fold toggle's own row below the text (a `text-10` control with its
 *  leading). Present only when the answer reaches the clamp. */
const FOLD_TOGGLE_H = 22;
/** `ChatNoticeEventRow` at the run's tail under `mt-1`, for the router's open
 *  question. A card, so it is a two-line shell plus its padding. */
const ASK_CARD_H = 88;
/** The chat prose's line box: `blockLayout`'s `LINE_HEIGHT_PX` (22) is the
 *  transcript's own answer for the same measurement (14px prose, leading 1.6).
 *  Not imported through `blockLayout` because that file documents the
 *  EXECUTION surface's numbers; this is the transcript's copy of the same
 *  measurement and must move with it. */
const ANSWER_LINE_H = 22;
/** Characters per line, the convention the bubble estimate beside this one
 *  already uses (`estimateRow` in `useTranscriptLayout`). Coarse on purpose:
 *  the fold ceiling, not the wrapping, is what bounds a long answer. */
const ANSWER_CHARS_PER_LINE = 70;

export interface RunHeightFacts {
  /** The run's newest answer body, when it is already known to the caller.
   *  Omitted (or empty) when the block has not been read yet — the common case
   *  for a row that has never been mounted — or when the run has not answered. */
  answerBody?: string | null;
  /** The router's open question is painted under the block. */
  pendingAsk?: boolean;
  /** Whether the block wears the agent header above its card. Every Chat
   *  surface passes `true` for a run item (a block always starts a turn), so
   *  the default is `true`; it is a parameter only so the estimate stays
   *  correct if a surface ever folds the header. */
  showHeader?: boolean;
}

/** The height of one collapsed run block, in px. */
export function estimateRunItemHeight(facts: RunHeightFacts = {}): number {
  const showHeader = facts.showHeader ?? true;
  const chrome =
    ITEM_PADDING_Y +
    (showHeader ? AGENT_HEADER_HEIGHT + HEADER_MARGIN_TOP : 0) +
    CARD_BORDER_Y +
    TITLE_BAR_H +
    ANSWER_BODY_PADDING_Y;

  // The answer is the only part big enough to matter, and the fold caps it.
  // Without the body, assume the cap bites: a run's answer is prose, and the
  // rows this estimate is used for have never been on screen — an answer short
  // enough NOT to overflow is the case where the over-estimate is smallest.
  const body = facts.answerBody ?? "";
  const lines =
    body.length === 0
      ? // Unknown, not short: a run whose answer has not been read yet must not
        // be sized as if it had one line — that is the under-estimate that made
        // a group window's total height a fraction of the truth.
        RUN_ANSWER_COLLAPSED_LINES + 1
      : Math.max(1, Math.ceil(body.length / ANSWER_CHARS_PER_LINE));
  const answerH = Math.max(
    1,
    Math.min(lines * ANSWER_LINE_H, collapsedHeightPx(ANSWER_LINE_H)),
  );
  const overflows = answerH >= collapsedHeightPx(ANSWER_LINE_H);

  return Math.round(
    chrome + answerH + (overflows ? FOLD_TOGGLE_H : 0) + (facts.pendingAsk ? ASK_CARD_H : 0),
  );
}