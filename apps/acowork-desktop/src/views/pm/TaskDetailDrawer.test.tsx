/**
 * Self-check for the TaskDetailDrawer header layout.
 *
 * What broke (and what this pins):
 *   The header holds TWO stacked zones — the title row (h2 + badge row +
 *   close button) and the tab strip (`role="tablist"`). A dialog-chrome
 *   unification pass (commit f97dc5ca) gave every dialog header the
 *   single-line recipe `flex items-center justify-between`. Copied here
 *   verbatim, that made `<header>` a flex ROW: the title block and the tab
 *   strip landed SIDE BY SIDE, both squashed into the 480px drawer. A long
 *   title wrapped one word per line, the tab strip overflowed the drawer,
 *   and it covered the overview fields below it.
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

import { TaskDetailDrawer } from "./TaskDetailDrawer";

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

describe("TaskDetailDrawer header", () => {
  beforeEach(() => {
    mocks.detail = task();
  });

  it("stacks the title row and the tab strip in a column, not a row", () => {
    render(<TaskDetailDrawer taskId="t-1" onClose={vi.fn()} />);

    const header = screen.getByRole("tablist").closest("header");
    expect(header, "tablist must live inside the drawer header").not.toBeNull();

    // Regression: `flex ... justify-between` (the single-line dialog recipe)
    // put title and tab strip side by side inside 480px.
    expect(header!.className).toContain("flex-col");
    expect(header!.className).not.toContain("justify-between");
    expect(header!.className).not.toMatch(/\bitems-(start|center|end)\b/);
  });

  it("puts the title row in its own row element inside the header", () => {
    render(<TaskDetailDrawer taskId="t-1" onClose={vi.fn()} />);

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
    render(<TaskDetailDrawer taskId="t-1" onClose={vi.fn()} />);

    const tablist = screen.getByRole("tablist");
    const tabs = screen.getAllByRole("tab");
    expect(tabs.length).toBeGreaterThan(2);
    for (const tab of tabs) expect(tablist.contains(tab)).toBe(true);
  });

  it("paginates tabs with arrow keys and wraps around", async () => {
    render(<TaskDetailDrawer taskId="t-1" onClose={vi.fn()} />);

    const tabs = screen.getAllByRole("tab");
    expect(selected(tabs[0])).toBe("true");

    fireEvent.keyDown(tabs[0], { key: "ArrowRight" });
    await waitFor(() => expect(selected(tabs[1])).toBe("true"));

    fireEvent.keyDown(tabs[tabs.length - 1], { key: "ArrowRight" });
    await waitFor(() => expect(selected(tabs[0])).toBe("true"));
  });
});
