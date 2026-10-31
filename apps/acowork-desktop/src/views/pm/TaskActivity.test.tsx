/**
 * TaskActivity — the submission timeline on the task surface.
 *
 * The defect this file exists for (t-80cf07ec): after an agent submitted a
 * task, the task dialog showed nothing about it. The submission text lived
 * only in `task.result` (overwrite-per-submit) and the review `_comment` was
 * dropped by the server, so there was no timeline to render. These tests pin
 * the two halves of the fix:
 *
 * 1. Every entry is readable — who, what kind, and the full text (agent
 *    submission AND human reply), not a truncated preview.
 * 2. The reply box writes to the activity stream and does not eat what the
 *    user typed when the request fails.
 *
 * Labels are asserted through the mocked actor names / entry text rather than
 * translated strings: i18n resolves asynchronously and a label assertion would
 * be a test of the locale files.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { TaskActivitySection } from "./TaskActivity";
import type { PmAttachmentMeta, PmTaskActivity } from "../../lib/pm-types";

const AGENT = "422c3696-c762-4e19-b8fd-bdec5eb6b1bb";

vi.mock("./pmMembers", () => ({
  useActorLabel: (id: string | null) =>
    id === AGENT ? "SSE" : id === "human" ? "Nicholas" : null,
}));

vi.mock("../../lib/pm-api", () => ({
  attachmentUrl: (id: string) => `/api/pm/attachments/${id}`,
}));

function activity(over: Partial<PmTaskActivity> = {}): PmTaskActivity {
  return {
    id: `act-${Math.random().toString(36).slice(2)}`,
    kind: "submitted",
    actor: AGENT,
    actor_kind: "agent",
    text: "",
    attachment_ids: [],
    created_at: "2026-10-30T08:40:00Z",
    ...over,
  };
}

const attachments: PmAttachmentMeta[] = [
  {
    id: "att-1",
    filename: "screenshot.png",
    kind: "image",
    content_type: "image/png",
    size: 2048,
    sha256: "x",
    storage_path: "attachments/att-1/original.png",
    thumb_path: null,
    width: null,
    height: null,
    uploaded_by: AGENT,
    uploaded_at: "2026-10-30T08:40:00Z",
  },
];

function renderSection(
  over: Partial<React.ComponentProps<typeof TaskActivitySection>> = {},
) {
  const onReply = vi.fn(async () => true);
  render(
    <TaskActivitySection
      activity={[]}
      attachments={attachments}
      replying={false}
      onReply={onReply}
      {...over}
    />,
  );
  return { onReply };
}

describe("TaskActivitySection", () => {
  beforeEach(() => vi.clearAllMocks());

  it("shows an agent submission with its author", () => {
    renderSection({
      activity: [activity({ text: "实现完成，测试全绿" })],
    });
    expect(screen.getByText("实现完成，测试全绿")).toBeTruthy();
    expect(screen.getByText("SSE")).toBeTruthy();
  });

  it("shows every entry, not just the last one", () => {
    renderSection({
      activity: [
        activity({ text: "第一版结果" }),
        activity({ kind: "rejected", actor: "human", actor_kind: "user", text: "少了回归测试" }),
        activity({ text: "第二版结果" }),
      ],
    });
    for (const text of ["第一版结果", "少了回归测试", "第二版结果"]) {
      expect(screen.getByText(text)).toBeTruthy();
    }
    expect(screen.getByText("Nicholas")).toBeTruthy();
  });

  it("links the attachments a submission referenced", () => {
    renderSection({
      activity: [activity({ text: "带图", attachment_ids: ["att-1", "att-unknown"] })],
    });
    const link = screen.getByTitle("screenshot.png").closest("a") as HTMLAnchorElement;
    expect(link.href).toContain("/api/pm/attachments/att-1");
    // 附件元数据还没同步到（列表比活动晚一拍）时，退回显示裸 id 而不是消失。
    expect(screen.getByTitle("att-unknown")).toBeTruthy();
  });

  it("sends a trimmed reply and clears the draft", async () => {
    const { onReply } = renderSection();
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: "  请先补一下截图  " } });
    fireEvent.click(screen.getByRole("button"));

    await waitFor(() => expect(onReply).toHaveBeenCalledWith("请先补一下截图"));
    await waitFor(() => expect((box as HTMLTextAreaElement).value).toBe(""));
  });

  it("keeps what the user typed when the reply fails", async () => {
    const onReply = vi.fn(async () => false);
    renderSection({ onReply });
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: "别丢我的字" } });
    fireEvent.click(screen.getByRole("button"));

    await waitFor(() => expect(onReply).toHaveBeenCalledTimes(1));
    expect((box as HTMLTextAreaElement).value).toBe("别丢我的字");
  });

  it("does not send an empty reply", () => {
    const { onReply } = renderSection();
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: "   " } });
    fireEvent.click(screen.getByRole("button"));
    expect(onReply).not.toHaveBeenCalled();
  });

  it("stays usable with no history at all", () => {
    renderSection();
    // 空历史也要留得住回复入口（人类可以在没提交过时先说一句话）。
    expect(screen.getByRole("textbox")).toBeTruthy();
  });
});
