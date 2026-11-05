import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ChatNoticeEventRow } from "./ChatNoticeEventRow";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

// `t` echoes the key (plus its interpolated params) so assertions read against
// the i18n vocabulary directly.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, options?: Record<string, unknown>) =>
      options ? `${key}:${Object.values(options).map(String).join(",")}` : key,
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

  it("hides EVERY retired milestone the mobile surface hides, body or not", () => {
    // Design 27 §5.5.4: Desktop's two views and Mobile share one hidden set. The
    // routing reaction and the card-resolution receipt are on it too — an old
    // transcript that carries them must not narrate a routing decision or a
    // decision card the reference row and the block now own (D8/D12). Asserted
    // WITH a body, because a bodyless row would fall through to nothing anyway.
    for (const noticeType of [
      "turn_dispatched",
      "turn_started",
      "turn_routed",
      "router_fallback",
      "interaction_resolved",
    ]) {
      const { container, unmount } = render(
        <ChatNoticeEventRow row={noticeRow(noticeType, "Routed to a new task")} />,
      );
      expect(container.innerHTML, noticeType).toBe("");
      unmount();
    }
  });

  it("paints nothing for a card with no options to offer", () => {
    // A card whose projection carries no options is retired chrome: nothing to
    // choose, nothing chosen. Painting it as a centered "Continue this task?"
    // put a question into the transcript that the agent never asked and nobody
    // could answer — the naked line was the most visible thing on the screen.
    const { container } = render(<ChatNoticeEventRow row={noticeRow("confirm_continue")} />);
    expect(container.innerHTML).toBe("");
    const { container: choice } = render(<ChatNoticeEventRow row={noticeRow("session_choice")} />);
    expect(choice.innerHTML).toBe("");
  });

  it("paints nothing once the question has been answered", () => {
    // A card is a question, and an answered question is an intermediate state of
    // that turn — ADR-106 D14 keeps it on disk and hides it at render time. What
    // a receipt would repeat ("handed to <task>"), the round's block already
    // says with more: rounds, state, and a way in. Three test messages used to
    // stack three identical receipt lines here.
    const row = noticeRow("confirm_continue");
    row.delivery = {
      answered: false,
      question: {
        answered: true,
        options: [
          { session_id: "exec-7", label: "整理报表", why: "最近活跃" },
          { session_id: "exec-8", label: "另一条线索" },
        ],
        allow_new_task: true,
      },
    };
    const { container } = render(<ChatNoticeEventRow row={row} onChoose={() => {}} />);
    expect(container.innerHTML).toBe("");
  });

  it("shows a viewer who may not answer the options without the buttons (D10)", () => {
    // Only the person who spoke may decide where their words go. A read-only
    // viewer still gets the question — it is a fact about the channel — and the
    // reason it is not theirs to answer, rather than a dead button.
    const row = noticeRow("session_choice");
    row.delivery = {
      answered: false,
      question: {
        answered: false,
        options: [
          { session_id: "exec-7", label: "整理报表", status: "idle" },
          { session_id: "exec-8", label: "另一条线索", status: "working" },
        ],
        allow_new_task: true,
      },
    };
    render(<ChatNoticeEventRow row={row} />);
    expect(screen.queryByRole("button")).toBeNull();
    expect(screen.getByText("整理报表")).toBeTruthy();
    expect(screen.getByText("chatView.block.answeredByOwner")).toBeTruthy();
  });

  it("makes every candidate row a button and the escape a footer action", () => {
    // A bordered box holding two lines of text reads as a disabled field: the
    // row has to BE the button, and "new task" is the way out of the
    // suggestion — never a second candidate of equal weight.
    const row = noticeRow("session_choice");
    row.delivery = {
      answered: false,
      question: {
        answered: false,
        options: [
          { session_id: "exec-7", label: "整理报表", why: "你刚说的那个" },
          { session_id: "exec-8", label: "另一条线索" },
        ],
        allow_new_task: true,
      },
    };
    render(<ChatNoticeEventRow row={row} onChoose={() => {}} />);
    const buttons = screen.getAllByRole("button");
    expect(buttons).toHaveLength(3); // two candidates + the escape
    expect(buttons[0].textContent).toContain("整理报表");
    expect(buttons[0].textContent).toContain("你刚说的那个");
    expect(buttons[2].textContent).toContain("chatView.block.newTask");
    // The header states that the question still wants an answer.
    expect(screen.getByText("chatView.block.unanswered")).toBeTruthy();
  });

  it("names the session a `confirm_continue` card would continue (D6 level 5)", () => {
    // The card offers exactly one candidate, so the button IS the answer. It used
    // to read a fixed "Continue this one" and drop the candidate's title and the
    // router's reason (both of which the payload carries) — the human was asked
    // to pick a task they could not identify.
    const row = noticeRow("confirm_continue");
    row.delivery = {
      answered: false,
      question: {
        answered: false,
        options: [
          {
            session_id: "exec-7",
            label: "发了个用户消息给SSE.kevin的chat session",
            why: "这是最近活跃的执行 session（4 分钟前刚收尾）",
          },
        ],
        allow_new_task: true,
      },
    };
    render(<ChatNoticeEventRow row={row} onChoose={() => {}} />);
    expect(screen.getByText("发了个用户消息给SSE.kevin的chat session")).toBeTruthy();
    expect(screen.getByText("这是最近活跃的执行 session（4 分钟前刚收尾）")).toBeTruthy();
    // The blind wording is gone from the labelled case.
    expect(screen.queryByText("chatView.block.chooseThis")).toBeNull();
  });

  it("falls back to the generic wording only when the option carries no label", () => {
    // An option that arrived without a title still has to be clickable and
    // nameable: the short session id is the last honest handle, and the fixed
    // wording stands in when even that is absent.
    const row = noticeRow("confirm_continue");
    row.delivery = {
      answered: false,
      question: {
        answered: false,
        options: [{ session_id: "exec-7" }],
        allow_new_task: false,
      },
    };
    render(<ChatNoticeEventRow row={row} onChoose={() => {}} />);
    // The session id is the last honest handle before the generic wording.
    expect(screen.getByText("exec-7")).toBeTruthy();
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
