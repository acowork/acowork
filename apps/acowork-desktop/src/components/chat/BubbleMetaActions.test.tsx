/**
 * BubbleMetaActions.test.tsx — the hover-revealed cluster left of a user
 * bubble's timestamp.
 *
 * The four chat surfaces used to hand-write their meta line, and the rule
 * that matters is not the icon set but the INERT state: a control that is
 * only transparent still takes clicks and still reserves width. These tests
 * pin the three behaviours that component exists to guarantee — the order
 * (resend, copy, time), the copy confirmation outliving the pointer, and
 * resend being absent on the surfaces that have nothing to re-send.
 */

import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";

// Locale-independent: t() echoes the key, so the assertions below pin the
// i18n hooks rather than one language's wording.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

import { act, fireEvent, render, screen } from "@testing-library/react";
import { BubbleMetaActions } from "./BubbleMetaActions";

// The cluster asks for itself whether the session may be written to (see
// `session-write-access.ts`), so the tests state the answer rather than
// depending on whatever the real stores happen to hold.
let readOnly = false;
vi.mock("../../lib/session-write-access", () => ({
  useActiveSessionReadOnly: () => readOnly,
}));

const writeText = vi.fn();

beforeEach(() => {
  writeText.mockReset();
  writeText.mockResolvedValue(undefined);
  Object.assign(navigator, { clipboard: { writeText } });
  readOnly = false;
});

afterEach(() => {
  vi.useRealTimers();
});

/** The cluster is the element that collapses; its buttons are the children. */
function clusterOf(): HTMLElement {
  return screen.getByLabelText("chatPanel.copy").parentElement as HTMLElement;
}

describe("BubbleMetaActions — composition", () => {
  it("puts resend to the left of copy", () => {
    render(<BubbleMetaActions copyText="hello" onResend={() => {}} />);
    const buttons = Array.from(clusterOf().children) as HTMLElement[];
    expect(buttons.map((b) => b.getAttribute("aria-label"))).toEqual([
      "chatPanel.resend",
      "chatPanel.copy",
    ]);
  });

  // The chat surfaces render a chat-log row, not a turn in an agent session:
  // handing them no handler must remove the affordance, not disable it.
  it("renders copy alone when there is nothing to re-send", () => {
    render(<BubbleMetaActions copyText="hello" />);
    expect(screen.queryByLabelText("chatPanel.resend")).toBeNull();
    expect(screen.getByLabelText("chatPanel.copy")).toBeTruthy();
  });

  it("renders nothing when it has no affordance to offer", () => {
    const { container } = render(<BubbleMetaActions copyText="" onResend={undefined} />);
    expect(container.textContent).toBe("");
  });
});

describe("BubbleMetaActions — copy", () => {
  it("copies the bubble body it was handed", async () => {
    render(<BubbleMetaActions copyText="段落 [[r1]]" />);
    await act(async () => {
      fireEvent.click(screen.getByLabelText("chatPanel.copy"));
    });
    expect(writeText).toHaveBeenCalledWith("段落 [[r1]]");
  });

  /** The reveal is `opacity-0` until the bubble group is hovered, so a
   *  confirmation that faded with the pointer would report nothing. The
   *  cluster therefore unfurles on `copied` as well as on hover — and the
   *  only way to assert that without a layout engine is the class rule. */
  it("keeps the cluster unfurled for the whole confirmation", async () => {
    vi.useFakeTimers();
    render(<BubbleMetaActions copyText="hello" />);
    const cluster = clusterOf();
    expect(cluster.className).not.toMatch(/(^|\s)opacity-100(\s|$)/);

    await act(async () => {
      fireEvent.click(screen.getByLabelText("chatPanel.copy"));
    });
    expect(cluster.className).toMatch(/opacity-100/);

    // And it closes on the same clock the button resets on.
    await act(async () => {
      vi.advanceTimersByTime(2000);
    });
    expect(cluster.className).not.toMatch(/(^|\s)opacity-100(\s|$)/);
  });

  // Collapsed to zero width rather than merely transparent: an invisible
  // button that still takes clicks is how a control got stranded mid-bubble.
  it("is collapsed and unclickable until the bubble is hovered", () => {
    render(<BubbleMetaActions copyText="hello" onResend={() => {}} />);
    const cluster = clusterOf();
    expect(cluster.className).toMatch(/\bw-0\b/);
    expect(cluster.className).toMatch(/overflow-hidden/);
    expect(cluster.className).toMatch(/group-hover:w-auto/);
  });
});

describe("BubbleMetaActions — resend", () => {
  it("calls the handler it was given", () => {
    const onResend = vi.fn();
    render(<BubbleMetaActions copyText="hello" onResend={onResend} />);
    fireEvent.click(screen.getByLabelText("chatPanel.resend"));
    expect(onResend).toHaveBeenCalledTimes(1);
  });

  /** A read-only session is the case the repo has been bitten by before:
   *  `session-write-access.ts` records three write controls that stayed
   *  enabled because the flag was threaded into children instead of being
   *  asked for by the control. The icon asks, so it cannot be forgotten. */
  it("drops resend on a read-only session but keeps copy", () => {
    readOnly = true;
    render(<BubbleMetaActions copyText="hello" onResend={() => {}} />);
    expect(screen.queryByLabelText("chatPanel.resend")).toBeNull();
    expect(screen.getByLabelText("chatPanel.copy")).toBeTruthy();
  });
});
