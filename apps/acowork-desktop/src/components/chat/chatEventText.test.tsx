import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ChatEventText } from "./ChatEventText";
import { ChatNoticeEventRow } from "./ChatNoticeEventRow";
import { ChatTurnStatusLine } from "./ChatTurnStatusLine";
import { InlineInteractionCard } from "./InlineInteractionCard";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, string>) =>
      params && params.question !== undefined ? `${key}«${params.question}»` : key,
  }),
}));

// jsdom has no layout engine, so "centered" / "in the row's content column" can
// only be asserted as the class that produces it. That is the point: the axis
// is the contract, and these tests pin which tier each call site claims.

describe("ChatEventText — the two tiers of the transcript's event text", () => {
  it("turn tier: centered, icon-less, and it carries the wait shimmer", () => {
    render(<ChatEventText anchor="turn" text="interrupted" live />);
    const el = document.querySelector('[data-event-text="turn"]') as HTMLElement;
    expect(el.className).toContain("justify-center");
    expect(el.querySelector("svg")).toBeNull();
    expect(el.getAttribute("role")).toBe("status");
    expect(el.textContent).toContain("interrupted");
    expect(el.querySelector(".thinking-shimmer")).toBeTruthy();
  });

  it("turn tier: a settled outcome is quiet, and danger only when asked", () => {
    render(<ChatEventText anchor="turn" text="failed" danger />);
    const el = document.querySelector('[data-event-text="turn"]') as HTMLElement;
    expect(el.getAttribute("role")).toBeNull();
    expect(el.querySelector(".thinking-shimmer")).toBeNull();
    expect(el.querySelector(".text-red-500")).toBeTruthy();
  });

  it("row tier: in the row's content column and led by the kind glyph", () => {
    render(<ChatEventText anchor="row" text="unanswered" icon={<svg data-testid="glyph" />} title="full" />);
    const el = document.querySelector('[data-event-text="row"]') as HTMLElement;
    expect(el.className).toContain("ml-12");
    expect(el.className).not.toContain("justify-center");
    expect(el.querySelector("svg")).toBeTruthy();
    expect(el.getAttribute("title")).toBe("full");
  });
});

// The grammar's real failure mode is a call site picking the wrong tier, so the
// two axes are asserted at the call sites, not only on the primitive.
describe("event-text grammar at the call sites", () => {
  it("a turn outcome ('interrupted') is a turn-tier line — no bubble, no row", () => {
    render(<ChatTurnStatusLine text="chatView.turnAborted" onJump={vi.fn()} />);
    const el = document.querySelector('[data-event-text="turn"]') as HTMLElement;
    expect(el).toBeTruthy();
    expect(el.className).toContain("justify-center");
    expect(document.querySelector('[data-event-text="row"]')).toBeNull();
  });

  it("a channel receipt is a turn-tier line (it belongs to no row)", () => {
    const row: ChatMessageRow = {
      seq: 9,
      ts: 1,
      from: "sys",
      kind: "system_notice",
      // The receipt's wording is whatever the runtime recorded in the body.
      body: "delivery failed",
      to: "u",
      meta: { notice_type: "delivery_failed" },
    };
    render(<ChatNoticeEventRow row={row} className="px-4" />);
    const el = document.querySelector('[data-event-text="turn"]') as HTMLElement;
    expect(el.className).toContain("justify-center");
    expect(el.className).toContain("px-4"); // the surface gutter survives
    expect(el.textContent).toContain("delivery failed");
  });

  it("a card closed without an answer is a row-tier line — it stays in the card's slot", () => {
    const row: ChatMessageRow = {
      seq: 1,
      ts: 1,
      from: "ag",
      kind: "interaction_request",
      body: "",
      to: "u",
      meta: {
        message_id: "card1",
        interaction: { kind: "approval", request_id: "a1", question: "Approve `bash`?" },
      },
    };
    render(
      <InlineInteractionCard
        row={row}
        closed={new Set(["card1"])}
        answered={new Set()}
        onAnswer={vi.fn()}
      />,
    );
    const el = document.querySelector('[data-event-text="row"]') as HTMLElement;
    expect(el).toBeTruthy();
    expect(el.className).toContain("ml-12");
    expect(el.className).not.toContain("justify-center");
    // The glyph is what distinguishes this tier from a turn-level line.
    expect(el.querySelector("svg")).toBeTruthy();
    expect(screen.getByText(/chatView\.cardUnanswered/)).toBeTruthy();
    // …and the card shell is gone: this is a note, not an unanswered card.
    expect(document.querySelector('[data-event-text="turn"]')).toBeNull();
    expect(screen.queryByText("chatView.cardApproval")).toBeNull();
  });
});