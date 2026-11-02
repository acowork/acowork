import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ChatNoticeEventRow } from "./ChatNoticeEventRow";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

// `t` echoes the key (plus its interpolated params) so assertions read against
// the i18n vocabulary directly.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, options?: Record<string, unknown>) =>
      options?.name ? `${key}:${String(options.name)}` : key,
  }),
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
  it.each(["turn_dispatched", "turn_started"])(
    "paints nothing for the retired %s row, even in an old transcript",
    (noticeType) => {
      // ADR-100 revision: history records conversation, not progress. An old
      // transcript still carrying a milestone must not put it back on screen.
      const { container } = render(<ChatNoticeEventRow row={noticeRow(noticeType)} />);
      expect(container.innerHTML).toBe("");
    },
  );

  it("paints the addressing question it is asked in common (D6 levels 3 and 5)", () => {
    // `session_choice` / `confirm_continue` are empty-bodied notices whose real
    // content is the option list — the picker paints the buttons, and this line
    // is the question they share, localized on the surface rather than by the
    // server (ADR-105's rule).
    const { container } = render(<ChatNoticeEventRow row={noticeRow("session_choice")} />);
    expect(container.textContent).toBe("chatView.block.chooseSession");
    const { container: confirm } = render(<ChatNoticeEventRow row={noticeRow("confirm_continue")} />);
    expect(confirm.textContent).toBe("chatView.block.confirmSession");
  });
  it("paints a delivery_failed receipt even though the row is bodyless (ADR-105)", () => {
    // The Gateway writes no body — the prose would be server-side English plus
    // an internal error code — so the surface owns the wording. It MUST paint:
    // the whole point of the row is that the failure stops being invisible.
    const { container } = render(
      <ChatNoticeEventRow row={noticeRow("delivery_failed")} name="Architect" />,
    );
    expect(container.textContent).toBe("chatView.deliveryFailed:Architect");
  });

  it("falls back to the unnamed wording when the surface cannot resolve the target", () => {
    const { container } = render(<ChatNoticeEventRow row={noticeRow("delivery_failed")} />);
    expect(container.textContent).toBe("chatView.deliveryFailedNoTarget");
  });

  it("folds a retired card-resolution receipt into nothing (the block paints it)", () => {
    // ADR-106 D12: no writer for this notice is left, and an old transcript must
    // not narrate a card that is now shown inside the round's block.
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
