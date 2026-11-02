import { render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ChatEventText } from "./ChatEventText";
import { ChatNoticeEventRow } from "./ChatNoticeEventRow";
import { ChatTurnStatusLine } from "./ChatTurnStatusLine";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, string>) =>
      params && params.question !== undefined ? `${key}«${params.question}»` : key,
  }),
}));

// jsdom has no layout engine, so "centered" can only be asserted as the class
// that produces it. That is the point: the axis is the contract.
// ADR-106 D3/D8 leave the transcript exactly one tier of event text — the
// centered one. The row-anchored note (a card closed with nobody answering it)
// retired with the chat-side card row, so these tests now also pin that no call
// site reintroduces a second axis.

describe("ChatEventText — the transcript's single event-text tier", () => {
  it("is centered, icon-less, and carries the wait shimmer when live", () => {
    render(<ChatEventText text="interrupted" live />);
    const el = document.querySelector('[data-event-text="turn"]') as HTMLElement;
    expect(el.className).toContain("justify-center");
    expect(el.querySelector("svg")).toBeNull();
    expect(el.getAttribute("role")).toBe("status");
    expect(el.textContent).toContain("interrupted");
    expect(el.querySelector(".thinking-shimmer")).toBeTruthy();
  });

  it("a settled outcome is quiet, and danger only when asked", () => {
    render(<ChatEventText text="failed" danger />);
    const el = document.querySelector('[data-event-text="turn"]') as HTMLElement;
    expect(el.getAttribute("role")).toBeNull();
    expect(el.querySelector(".thinking-shimmer")).toBeNull();
    expect(el.querySelector(".text-red-500")).toBeTruthy();
  });

  it("a trailing affordance rides the same line (the block's retry)", () => {
    render(<ChatEventText text="unreachable" trailing={<button>retry</button>} />);
    const el = document.querySelector('[data-event-text="turn"]') as HTMLElement;
    expect(el.querySelector("button")?.textContent).toBe("retry");
  });
});

// The grammar's real failure mode is a call site stepping off the axis, so the
// centered line is asserted at the call sites, not only on the primitive.
describe("event-text grammar at the call sites", () => {
  it("a turn outcome ('interrupted') is a centered line — no bubble, no row note", () => {
    render(<ChatTurnStatusLine text="chatView.turnAborted" />);
    const el = document.querySelector('[data-event-text="turn"]') as HTMLElement;
    expect(el).toBeTruthy();
    expect(el.className).toContain("justify-center");
  });

  it("a channel receipt is a centered line (it belongs to no row)", () => {
    const row: ChatMessageRow = {
      seq: 9,
      ts: 1,
      from: "sys",
      kind: "system_notice",
      // ADR-105: the Gateway writes this receipt bodyless and every surface
      // owns the wording, so the line paints from the notice type alone (a
      // legacy body-bearing row is re-worded rather than quoted verbatim).
      body: "",
      to: "u",
      meta: { notice_type: "delivery_failed" },
    };
    render(<ChatNoticeEventRow row={row} className="px-4" />);
    const el = document.querySelector('[data-event-text="turn"]') as HTMLElement;
    expect(el.className).toContain("justify-center");
    expect(el.className).toContain("px-4"); // the surface gutter survives
    expect(el.textContent).toContain("chatView.deliveryFailed");
  });
});
