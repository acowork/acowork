// @vitest-environment jsdom
/**
 * CollapsibleChatContent — the fold an over-long chat bubble wears.
 *
 * Why this file exists — the failures the fold must not reintroduce:
 *   1. a long answer that swallows the transcript. The fold exists to show
 *      the first ten lines and say that there is more; an unclamped bubble
 *      under every row turns the conversation into a wall of text, and the
 *      reader can no longer page through the shape of the conversation;
 *   2. a fold that cannot be opened — a chevron clipped by the very overflow
 *      it controls is a control the reader cannot reach;
 *   3. a live refresh that snaps the content shut under the reader's finger.
 *      A session keeps talking; the answer grows; the reader's choice to read
 *      the whole thing must survive that. The ONE exception is content that
 *      no longer needs a fold: there is nothing to expand, so the resting
 *      shape is the whole content again;
 *   4. a page jump on expand. The clip animates its ceiling instead of
 *      snapping between two heights.
 *   5. a knifed cut. Folded, the clip's bottom band dissolves the text
 *      through a mask; expanded, it must NOT still be fading the last line.
 *
 * The environment here has no layout (jsdom), so the fold takes its
 * measurement fallback path — the length estimate. That is a deliberate
 * subject of these tests: the DOM-less path must still decide "more to read"
 * correctly, or the fold would never appear in a headless environment and
 * the browser path would be the only one ever exercised.
 */
import { fireEvent, render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { CollapsibleChatContent } from "./CollapsibleChatContent";
import {
  FOLD_FADE_BAND_PX,
  RUN_ANSWER_COLLAPSED_LINES,
  collapsedHeightPx,
  isOverflowing,
  resolveExpanded,
  resolveLineHeightPx,
} from "./chatBubbleFold";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

/** Content long enough to be folded: many lines of text. */
function longText(lines = 40): string {
  return Array.from({ length: lines }, (_, i) => `line ${i + 1}`).join("\n");
}

/** The clip's own element — the box whose ceiling is the fold. */
function clip(container: HTMLElement): HTMLElement {
  return container.querySelector("[data-chat-fold-clip]") as HTMLElement;
}

/** The toggle, if the fold painted one. */
function toggle(container: HTMLElement): HTMLElement | null {
  return container.querySelector("[data-chat-fold-toggle]");
}

describe("CollapsibleChatContent", () => {
  it("shows short content whole, with nothing to expand", () => {
    const { container } = render(
      <CollapsibleChatContent text="short answer">
        <div>short answer</div>
      </CollapsibleChatContent>,
    );
    expect(container.textContent).toContain("short answer");
    // No chevron: there is nothing more to read, so a fold control would be
    // a control that does nothing.
    expect(toggle(container)).toBeNull();
  });

  it("folds long content and offers a way to read the rest", () => {
    const text = longText();
    const { container } = render(
      <CollapsibleChatContent text={text}>
        <div>{text}</div>
      </CollapsibleChatContent>,
    );
    const btn = toggle(container);
    expect(btn).not.toBeNull();
    expect(btn?.getAttribute("aria-expanded")).toBe("false");
    // The fold is a real ceiling, not a marker.
    expect(clip(container).style.maxHeight).not.toBe("");
  });

  it("the chevron opens the whole content and closes it again", () => {
    const text = longText();
    const { container } = render(
      <CollapsibleChatContent text={text}>
        <div>{text}</div>
      </CollapsibleChatContent>,
    );
    fireEvent.click(toggle(container)!);
    expect(toggle(container)?.getAttribute("aria-expanded")).toBe("true");
    // The expanded ceiling is taller than the folded one.
    const expanded = parseFloat(clip(container).style.maxHeight);
    fireEvent.click(toggle(container)!);
    const collapsed = parseFloat(clip(container).style.maxHeight);
    expect(collapsed).toBeLessThan(expanded);
    expect(toggle(container)?.getAttribute("aria-expanded")).toBe("false");
  });

  it("the toggle carries a reachable hit area and a label", () => {
    const text = longText();
    const { container } = render(
      <CollapsibleChatContent text={text}>
        <div>{text}</div>
      </CollapsibleChatContent>,
    );
    const btn = toggle(container)!;
    // A chevron alone is a small target; the control is a padded button.
    expect(btn.className).toContain("h-7");
    // It keeps its own row below the text in BOTH states: expanding must
    // stay a pure height change, with no page jump and no arrow on the
    // answer's last line.
    expect(btn.className).toContain("mt-1");
    expect(btn.className).not.toContain("absolute");
    // Idle: no rectangle is drawn over the bubble. The rounded highlight
    // is a pointer affordance, painted only on hover / keyboard focus.
    expect(btn.className).toContain("bg-transparent");
    expect(btn.className).toContain("hover:bg-black/5");
    expect(btn.className).not.toContain("bg-linear-to-b");
    expect(btn.getAttribute("aria-label")).toBe("chatView.block.expandAnswer");
    fireEvent.click(btn);
    expect(toggle(container)?.getAttribute("aria-label")).toBe(
      "chatView.block.collapseAnswer",
    );
  });

  it("fades the cut while folded and stops fading once expanded", () => {
    const text = longText();
    const { container } = render(
      <CollapsibleChatContent text={text}>
        <div>{text}</div>
      </CollapsibleChatContent>,
    );
    // Folded: the clip wears the mask whose bottom band dissolves the text.
    expect(clip(container).hasAttribute("data-chat-fold-fade")).toBe(true);
    expect(clip(container).style.maskImage).toContain("linear-gradient");
    expect(clip(container).style.maskImage).toContain(`${FOLD_FADE_BAND_PX}px`);
    fireEvent.click(toggle(container)!);
    // Expanded: no mask. Leaving it on would fade the answer's last line.
    expect(clip(container).hasAttribute("data-chat-fold-fade")).toBe(false);
    expect(clip(container).style.maskImage).toBe("");
    fireEvent.click(toggle(container)!);
    expect(clip(container).hasAttribute("data-chat-fold-fade")).toBe(true);
  });

  it("keeps the chevron below the text after expanding", () => {
    const text = longText();
    const { container } = render(
      <CollapsibleChatContent text={text}>
        <div>{text}</div>
      </CollapsibleChatContent>,
    );
    fireEvent.click(toggle(container)!);
    const btn = toggle(container)!;
    // A floating toggle would land on the answer's last line, where a
    // transparent control is illegible. The row must stay in normal flow.
    expect(btn.className).not.toContain("absolute");
    expect(btn.className).toContain("mt-1");
    expect(clip(container).contains(btn)).toBe(false);
  });

  it("a live refresh does not snap an expanded answer shut", () => {
    const { container, rerender } = render(
      <CollapsibleChatContent text={longText()}>
        <div>{longText()}</div>
      </CollapsibleChatContent>,
    );
    fireEvent.click(toggle(container)!);
    expect(toggle(container)?.getAttribute("aria-expanded")).toBe("true");
    // The session keeps talking: the answer grows.
    const grown = longText(60);
    rerender(
      <CollapsibleChatContent text={grown}>
        <div>{grown}</div>
      </CollapsibleChatContent>,
    );
    expect(toggle(container)?.getAttribute("aria-expanded")).toBe("true");
  });

  it("content that no longer needs a fold returns to the resting shape", () => {
    const { container, rerender } = render(
      <CollapsibleChatContent text={longText()}>
        <div>{longText()}</div>
      </CollapsibleChatContent>,
    );
    fireEvent.click(toggle(container)!);
    expect(toggle(container)?.getAttribute("aria-expanded")).toBe("true");
    // The content shrank to something that fits: there is nothing to expand,
    // so a stale "expanded" flag would be a lie the reader can act on.
    rerender(
      <CollapsibleChatContent text="short">
        <div>short</div>
      </CollapsibleChatContent>,
    );
    expect(toggle(container)).toBeNull();
  });

  it("the chevron sits outside the clip that hides the rest of the content", () => {
    const text = longText();
    const { container } = render(
      <CollapsibleChatContent text={text}>
        <div>{text}</div>
      </CollapsibleChatContent>,
    );
    const clipEl = clip(container);
    const btn = toggle(container)!;
    // A control inside the clipped box would be cut off with the text it
    // controls.
    expect(clipEl.contains(btn)).toBe(false);
  });
});

describe("chatBubbleFold", () => {
  it("counts ten lines as the ceiling", () => {
    expect(RUN_ANSWER_COLLAPSED_LINES).toBe(10);
    expect(collapsedHeightPx(20)).toBe(200);
    // The ceiling keeps those ten lines AND a fade band; the expanded
    // ceiling is the content's own full height, nothing reserved inside it.
    expect(FOLD_FADE_BAND_PX).toBe(48);
  });

  it("reads the browser's pixel line-height, and the shapes a hand-written style uses", () => {
    expect(resolveLineHeightPx("19.9px", 14)).toBeCloseTo(19.9);
    expect(resolveLineHeightPx("1.5", 14)).toBeCloseTo(21);
    expect(resolveLineHeightPx("1.5em", 14)).toBeCloseTo(21);
    expect(resolveLineHeightPx("normal", 14)).toBeCloseTo(14 * 1.6);
    expect(resolveLineHeightPx(null, 14)).toBeCloseTo(14 * 1.6);
  });

  it("a rounding difference is not 'more text'", () => {
    expect(isOverflowing(200, 200)).toBe(false);
    expect(isOverflowing(201, 200)).toBe(false);
    expect(isOverflowing(202, 200)).toBe(true);
  });

  it("an expansion survives a refresh, but only while there is something to expand", () => {
    expect(resolveExpanded(true, true)).toBe(true);
    expect(resolveExpanded(false, true)).toBe(false);
    expect(resolveExpanded(true, false)).toBe(false);
  });
});
