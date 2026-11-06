import { fireEvent, render, screen, waitFor } from "@testing-library/react";
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
      // ADR-105 D5 rev. 2026-11-06: the failed-send receipt is not stored any
      // more — it is a live `delivery.alert`. The rows history holds are the
      // retracted kind, so they must disappear with the rest of the milestones.
      "delivery_failed",
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

  it("paints nothing for a card nothing could ever answer", () => {
    // The 2026-11-06 defect, as a rule: a group `@` fan-out wrote the card under
    // the per-target delivery uuid instead of the human's row, so the card named
    // a coordinate the log never holds. It can never be answered — every click
    // came back `origin_gone`/422 — and a question that cannot be asked is no
    // more a conversation record than an answered one (ADR-106 D14). The log is
    // append-only, so those rows stay on disk and leave at render time.
    const row = noticeRow("session_choice");
    row.delivery = {
      answered: false,
      question: {
        answered: false,
        answerable: false,
        options: [
          { session_id: "exec-1", label: "优化 chat_prompt 的长度" },
          { session_id: "exec-2", label: "untitled" },
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

  // ── The card wears the agent's identity ─────────────────────────────

  /** A `confirm_continue` card with one candidate and the "new task" escape. */
  function answerableCard(noticeType = "confirm_continue"): ChatMessageRow {
    const row = noticeRow(noticeType);
    row.delivery = {
      answered: false,
      question: {
        answered: false,
        options: [{ session_id: "exec-7", label: "整理报表", why: "最近活跃" }],
        allow_new_task: true,
      },
    };
    return row;
  }

  it("speaks as the agent: avatar, name and role above the question", () => {
    // The card is the agent's own question about its own runs, not a channel
    // receipt, so it is painted as that agent's message — same header, same
    // offsets as the execution block that replaces it once the human picks.
    // Anything else made the answer arrive under a name that had never spoken.
    const { container } = render(
      <ChatNoticeEventRow row={answerableCard()} authorName="SSE" onChoose={() => {}} />,
    );
    expect(screen.getByText("SSE")).toBeTruthy();
    // An avatar, in the header, before the card.
    const avatar = container.querySelector("img, span.rounded-lg, div[style*='40px']");
    expect(avatar).toBeTruthy();
    const card = screen.getByTestId("chat-card-options");
    // The card sits in the same 40px gutter the agent bubble and the execution
    // block use, so the question and the run that follows start on one line.
    expect(card.querySelector(".w-10")).toBeTruthy();
    // The author is the row's `from` — the agent instance the projection named.
    expect(container.textContent).toContain("SSE");
  });

  it("keeps the card busy from the click until the answer lands", async () => {
    // "Click did nothing" was the complaint that a fire-and-forget click
    // produced: no state change, no visible reaction. The card now holds the
    // decision open — the pressed row spins, every other row and the escape go
    // dead — and leaves only when the projection says the question was answered.
    const row = answerableCard();
    let release: (v: boolean) => void = () => {};
    const onChoose = vi.fn(() => new Promise<boolean>((r) => (release = r)));
    render(<ChatNoticeEventRow row={row} onChoose={onChoose} />);

    const buttons = screen.getAllByRole("button");
    fireEvent.click(buttons[0]);
    expect(onChoose).toHaveBeenCalledWith({ sessionId: "exec-7" });
    // In flight: everything disabled, and the header says what is happening.
    for (const b of screen.getAllByRole("button")) {
      expect((b as HTMLButtonElement).disabled).toBe(true);
    }
    expect(screen.getByText("chatView.block.choosing")).toBeTruthy();

    // A landed delivery does NOT release the buttons: the card is answered away
    // by the projection, and a live question under the human's pointer after
    // they already decided is the flash this holds down.
    release(true);
    await Promise.resolve();
    await Promise.resolve();
    expect(screen.getByText("chatView.block.choosing")).toBeTruthy();
  });

  it("re-opens the card when the click delivered nothing", async () => {
    // A refused click (a card that is already gone, a send the Gateway would not
    // take) must not leave a dead question behind: `false` is the surface saying
    // "nothing was delivered", and the buttons come back.
    const row = answerableCard();
    const onChoose = vi.fn(async () => false);
    render(<ChatNoticeEventRow row={row} onChoose={onChoose} />);
    fireEvent.click(screen.getAllByRole("button")[1]); // the "new task" escape
    expect(onChoose).toHaveBeenCalledWith({ forceNew: true });
    await waitFor(() => {
      for (const b of screen.getAllByRole("button")) {
        expect((b as HTMLButtonElement).disabled).toBe(false);
      }
    });
    expect(screen.getByText("chatView.block.unanswered")).toBeTruthy();
  });

  it("re-opens the card when the click throws", async () => {
    const row = answerableCard();
    render(
      <ChatNoticeEventRow
        row={row}
        onChoose={() => Promise.reject(new Error("gateway down"))}
      />,
    );
    fireEvent.click(screen.getAllByRole("button")[0]);
    await waitFor(() => {
      expect(
        (screen.getAllByRole("button")[0] as HTMLButtonElement).disabled,
      ).toBe(false);
    });
  });
});
