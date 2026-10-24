import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ChatNoticeEventRow } from "./ChatNoticeEventRow";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

// `t` echoes the key so assertions read against the i18n vocabulary directly.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

function noticeRow(noticeType: string, body = ""): ChatMessageRow {
  return {
    seq: 1,
    ts: 1,
    from: "ag",
    kind: "system_notice",
    body,
    to: "u",
    meta: {
      message_id: `n:${noticeType}`,
      notice_type: noticeType,
      in_reply_to: "u1",
      agent_session_id: "exec-1",
    },
  };
}

describe("ChatNoticeEventRow", () => {
  it.each(["turn_dispatched", "turn_started", "router_fallback"])(
    "paints nothing for the retired %s row, even in an old transcript",
    (noticeType) => {
      // ADR-100 revision: history records conversation, not progress. An old
      // transcript still carrying a milestone must not put it back on screen.
      const { container } = render(<ChatNoticeEventRow row={noticeRow(noticeType)} />);
      expect(container.innerHTML).toBe("");
    },
  );

  it("folds an interaction_resolved receipt into nothing (the card paints it)", () => {
    const { container } = render(<ChatNoticeEventRow row={noticeRow("interaction_resolved")} />);
    expect(container.innerHTML).toBe("");
  });

  it("still renders a genuine channel receipt's body", () => {
    render(<ChatNoticeEventRow row={noticeRow("budget_exhausted", "Out of budget")} />);
    expect(screen.getByText("Out of budget")).toBeTruthy();
  });

  it("falls back to the status field when the body is empty", () => {
    const row = noticeRow("unavailable");
    row.meta = { ...row.meta, status: "agent unavailable" };
    render(<ChatNoticeEventRow row={row} />);
    expect(screen.getByText("agent unavailable")).toBeTruthy();
  });

  it("paints no hole when there is neither text nor status", () => {
    const { container } = render(<ChatNoticeEventRow row={noticeRow("")} />);
    expect(container.innerHTML).toBe("");
  });
});
