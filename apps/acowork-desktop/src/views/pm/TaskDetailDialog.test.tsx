/**
 * Self-check for the TaskDetailDialog header layout + centered chrome.
 *
 * What broke (and what this pins):
 *   The header holds TWO stacked zones — the title row (h2 + badge row +
 *   close button) and the tab strip (`role="tablist"`). A dialog-chrome
 *   unification pass (commit f97dc5ca) gave every dialog header the
 *   single-line recipe `flex items-center justify-between`. Copied here
 *   verbatim, that made `<header>` a flex ROW: the title block and the tab
 *   strip landed SIDE BY SIDE, both squashed into the panel. A long title
 *   wrapped one word per line, the tab strip overflowed the panel, and it
 *   covered the overview fields below it.
 *
 * Chrome contract:
 *   The dialog used to be a right-side slide-in drawer. It is now the
 *   app-wide centered dialog family (GlobalSearchDialog chrome): overlay
 *   `fixed inset-0 ... items-center justify-center` + centered panel —
 *   the pm/doc right rail belongs to the settings panel, not task detail.
 *
 * Why this reads class names instead of measuring:
 *   jsdom computes no layout, and the broken and fixed trees are the SAME
 *   DOM — only the header's flex direction differs. So the class list is
 *   the spec here, same approach as capsule.test.tsx and
 *   styles/globalFontSize.test.ts.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import type { PmTaskResponse } from "../../lib/pm-types";

const mocks = vi.hoisted(() => ({
  detail: null as PmTaskResponse | null,
  openTask: vi.fn(async () => {}),
  refresh: vi.fn(async () => {}),
  moveTask: vi.fn(async () => {}),
}));

vi.mock("../../stores/pm/taskDetailStore", () => ({
  usePmTaskDetailStore: (selector: (state: Record<string, unknown>) => unknown) =>
    selector({
      detail: mocks.detail,
      attachments: [],
      loading: false,
      uploading: false,
      error: null,
      openTask: mocks.openTask,
      refresh: mocks.refresh,
      uploadAttachment: vi.fn(),
      deleteAttachment: vi.fn(),
    }),
}));

vi.mock("../../stores/pm/boardStore", () => ({
  usePmBoardStore: (selector: (state: Record<string, unknown>) => unknown) =>
    selector({ tasks: [], removeTask: vi.fn(), moveTask: mocks.moveTask }),
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (state: Record<string, unknown>) => unknown) =>
    selector({ agents: {} }),
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

import { TaskDetailDialog } from "./TaskDetailDialog";

function task(over: Partial<PmTaskResponse> = {}): PmTaskResponse {
  return {
    id: "t-1",
    project_id: "p-1",
    title: "记忆召回需要增加user信息输入和LLM对召回信息汇总摘要",
    description: "desc",
    type: "feature",
    status: "in_progress",
    review_status: "approved",
    priority: "normal",
    assignee: "agent-1",
    due_at: null,
    start_at: null,
    dispatched_at: null,
    dispatch_attempts: 0,
    depends_on: [],
    attachments: [],
    result: null,
    created_by: "human",
    created_at: "2026-10-05T02:35:00Z",
    updated_at: "2026-10-05T02:35:00Z",
    claimed_at: null,
    submitted_at: null,
    is_blocked: false,
    blocked_by: [],
    depth: 0,
    parent_id: null,
    ...over,
  };
}

const selected = (el: Element) => el.getAttribute("aria-selected");

describe("TaskDetailDialog chrome", () => {
  beforeEach(() => {
    mocks.detail = task();
  });

  it("centers the panel on a full-screen overlay (no right-side drawer)", () => {
    render(<TaskDetailDialog taskId="t-1" onClose={vi.fn()} />);

    const panel = screen.getByRole("dialog");
    const overlay = panel.parentElement!;
    // Overlay: fixed full-screen flex centered + modal scrim color.
    expect(overlay.className).toContain("fixed");
    expect(overlay.className).toContain("inset-0");
    expect(overlay.className).toContain("items-center");
    expect(overlay.className).toContain("justify-center");
    expect(overlay.className).toContain("bg-modal-overlay");
    // Panel: unified centered-dialog chrome, not the old `inset-y-0 right-0`
    // drawer slot.
    expect(panel.className).toContain("max-w-3xl");
    expect(panel.className).toContain("bg-modal-surface");
    expect(panel.className).not.toContain("inset-y-0");
    expect(panel.className).not.toContain("right-0");
  });

  it("stacks the title row and the tab strip in a column, not a row", () => {
    render(<TaskDetailDialog taskId="t-1" onClose={vi.fn()} />);

    const header = screen.getByRole("tablist").closest("header");
    expect(header, "tablist must live inside the dialog header").not.toBeNull();

    // Regression: `flex ... justify-between` (the single-line dialog recipe)
    // put title and tab strip side by side inside the narrow panel.
    expect(header!.className).toContain("flex-col");
    expect(header!.className).not.toContain("justify-between");
    expect(header!.className).not.toMatch(/\bitems-(start|center|end)\b/);
  });

  it("puts the title row in its own row element inside the header", () => {
    render(<TaskDetailDialog taskId="t-1" onClose={vi.fn()} />);

    const title = screen.getByRole("heading", { level: 2 });
    const titleRow = title.closest("header")!.querySelector("header > div");
    // Title sits in the title row, NOT directly in the header (that row is
    // what carries the close button to the right edge).
    expect(titleRow).not.toBeNull();
    expect(titleRow!.contains(title)).toBe(true);
    // The tab strip is the row BELOW it.
    expect(titleRow!.nextElementSibling?.getAttribute("role")).toBe("tablist");
  });

  it("keeps every tab inside the strip with no sibling overflow row", () => {
    render(<TaskDetailDialog taskId="t-1" onClose={vi.fn()} />);

    const tablist = screen.getByRole("tablist");
    const tabs = screen.getAllByRole("tab");
    expect(tabs.length).toBeGreaterThan(2);
    for (const tab of tabs) expect(tablist.contains(tab)).toBe(true);
  });

  it("paginates tabs with arrow keys and wraps around", async () => {
    render(<TaskDetailDialog taskId="t-1" onClose={vi.fn()} />);

    const tabs = screen.getAllByRole("tab");
    expect(selected(tabs[0])).toBe("true");

    fireEvent.keyDown(tabs[0], { key: "ArrowRight" });
    await waitFor(() => expect(selected(tabs[1])).toBe("true"));

    fireEvent.keyDown(tabs[tabs.length - 1], { key: "ArrowRight" });
    await waitFor(() => expect(selected(tabs[0])).toBe("true"));
  });
});

describe("TaskDetailDialog dispatch failure surface", () => {
  beforeEach(() => {
    mocks.detail = task();
  });

  it("shows the persisted failure while the task is still undispatched", () => {
    mocks.detail = task({
      status: "pending",
      dispatched_at: null,
      dispatch_attempts: 3,
      dispatch_error: "gateway unreachable",
    });
    render(<TaskDetailDialog taskId="t-1" onClose={vi.fn()} />);
    // The mocked `t` echoes the key; the banner must be on the overview tab.
    expect(screen.getByText("pm.task.dispatchFailed")).toBeTruthy();
  });

  it("shows the abandoned variant once attempts reach the bound", () => {
    mocks.detail = task({
      status: "pending",
      dispatched_at: null,
      dispatch_attempts: 5,
      dispatch_error: "gateway unreachable",
    });
    render(<TaskDetailDialog taskId="t-1" onClose={vi.fn()} />);
    expect(screen.getByText("pm.task.dispatchAbandoned")).toBeTruthy();
  });

  it("stays silent after a successful dispatch", () => {
    mocks.detail = task({
      status: "pending",
      dispatched_at: "2026-10-25T01:00:00Z",
      dispatch_attempts: 0,
      dispatch_error: null,
    });
    render(<TaskDetailDialog taskId="t-1" onClose={vi.fn()} />);
    expect(screen.queryByText("pm.task.dispatchFailed")).toBeNull();
  });
});
