/**
 * Prompts & Skills card (Debug tab) — card grammar + tree behaviour.
 *
 * What this pins (the things that silently drift when the card is
 * hand-styled instead of using the shared list primitives):
 *
 *   1. The card Wears the app's box-card grammar — `ListBox` on the raised
 *      `bg-panel-block` surface, and a body on the `panel-inset` sub-surface
 *      below a hairline. A hand-rolled `border + header + div` shell reads as
 *      a different card family from the Settings / Status panels next to it.
 *   2. The header is the collapse toggle (ExpandableRow), and the reload
 *      button inside it must NOT toggle the card — an operator reloading
 *      prompts must not have the file list collapse under them.
 *   3. Explorer click grammar: directories toggle on a single click, files
 *      SELECT on a single click and OPEN on a double click. Single-click-to-
 *      open would skip the step the workspace tree trains the user on, and the
 *      two trees share one editor tab bar.
 *   4. Only the two editable subtrees are ever reachable: every open goes
 *      through `openFile` with the `__agent_package__` workspace id and a
 *      path built from the entry names walked so far (never from server data
 *      verbatim). Markdown files must reach Monaco in EDIT mode — the
 *      Explorer's `isWorkspacePreviewable` rule would send every one of them
 *      to preview, where Save is disabled.
 *   5. The selected file is highlighted, so the card shows where a click landed.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";

const h = vi.hoisted(() => ({
  openFile: vi.fn(),
  invalidate: vi.fn(),
  fetchTreeNode: vi.fn(),
  fetch: vi.fn(),
  reloadResponse: { ok: true, status: 200, statusText: "OK" } as { ok: boolean; status: number; statusText: string },
}));

function selectorStore<T>(state: T) {
  return Object.assign((selector: (s: T) => unknown) => selector(state), { getState: () => state });
}

vi.mock("../../stores/fileEditorStore", () => ({
  useFileEditorStore: selectorStore({ openFile: h.openFile, activeFileId: null as string | null }),
}));

vi.mock("../../stores/fileTree/fileTreeStore", () => ({
  useFileTreeStore: selectorStore({ invalidate: h.invalidate }),
  fetchTreeNode: h.fetchTreeNode,
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: selectorStore({ selectedAgentId: "com.test.agent" }),
}));

vi.mock("../../stores/debugStore", () => ({
  useDebugStore: selectorStore({ debugAgentId: null as string | null }),
}));

vi.mock("../../lib/config", () => ({
  getGatewayUrl: () => "http://127.0.0.1:19876",
}));

import { PromptsSkillsCard } from "./PromptsSkillsCard";

/** Root listing the two whitelisted subtrees, as the Runtime serves it. */
const ROOT = {
  kind: "ready" as const,
  root: "C:/pkg",
  fetchedAt: Date.now(),
  entries: [
    { name: "prompts", type: "directory" as const },
    { name: "skills", type: "directory" as const },
  ],
};

const SKILL_DIR = {
  kind: "ready" as const,
  root: "C:/pkg/skills",
  fetchedAt: Date.now(),
  entries: [{ name: "greeting", type: "directory" as const }],
};

const PROMPTS_DIR = {
  kind: "ready" as const,
  root: "C:/pkg/prompts",
  fetchedAt: Date.now(),
  entries: [{ name: "system.md", type: "file" as const }],
};

const SKILL_FILE = {
  kind: "ready" as const,
  root: "C:/pkg/skills/greeting",
  fetchedAt: Date.now(),
  entries: [{ name: "SKILL.md", type: "file" as const }],
};

/**
 * `listRowSelectedInset` only (the unselected `listRowHoverInset` carries a
 * `dark:hover:bg-.../20` variant, so matching on "accent)]/20" alone would
 * hit every row in the card).
 */
const SELECTED_FILL = "dark:bg-[var(--color-accent)]/25";

describe("PromptsSkillsCard", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    h.fetchTreeNode.mockImplementation(async (_agent: string, _ws: string, relPath: string) => {
      if (relPath === "") return ROOT;
      if (relPath === "skills") return SKILL_DIR;
      if (relPath === "skills/greeting") return SKILL_FILE;
      if (relPath === "prompts") return PROMPTS_DIR;
      return { kind: "ready" as const, root: "C:/pkg", fetchedAt: Date.now(), entries: [] };
    });
    vi.stubGlobal("fetch", vi.fn(async () => h.reloadResponse));
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("wears the shared box-card grammar with an inset body", async () => {
    const { container } = render(<PromptsSkillsCard />);
    await screen.findByText("Prompts & Skills");

    const card = container.firstElementChild as HTMLElement;
    // Raised box card — same chrome as every Settings / Status card.
    expect(card.className).toContain("bg-panel-block");
    expect(card.className).toContain("rounded-md");
    expect(card.className).toContain("border");

    // Body sits on the inset sub-surface below a hairline.
    await screen.findByText("prompts");
    const body = container.querySelector(".bg-panel-inset") as HTMLElement;
    expect(body).not.toBeNull();
    expect(body.className).toContain("border-t");
    expect(body.className).toContain("rounded-b-md");
    // Rows live inside that inset body, not on the card surface above it.
    expect(body.contains(screen.getByText("prompts"))).toBe(true);
  });

  it("collapses the file list from the header row and keeps the reload button inert", async () => {
    const { container } = render(<PromptsSkillsCard />);
    await screen.findByText("prompts");

    const header = screen.getByRole("button", { name: "Prompts & Skills" });
    expect(header.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(header);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Prompts & Skills" }).getAttribute("aria-expanded")).toBe("false"),
    );
    expect(screen.queryByText("prompts")).toBeNull();
    // Collapsed card must shrink back to the header (ListBox drops flex-1).
    expect((container.firstElementChild as HTMLElement).className).not.toContain("flex-1");

    // Reload must not double as the collapse toggle.
    await fireEvent.click(screen.getByRole("button", { name: "Reload prompts & skills" }));
    await waitFor(() => expect(globalThis.fetch).toHaveBeenCalled());
    expect(screen.getByRole("button", { name: "Prompts & Skills" }).getAttribute("aria-expanded")).toBe("false");
  });

  it("expands directories on a single click", async () => {
    render(<PromptsSkillsCard />);
    await screen.findByText("prompts");

    fireEvent.click(screen.getByText("skills"));
    expect(await screen.findByText("greeting")).toBeTruthy();
    // A directory click must never reach the editor.
    expect(h.openFile).not.toHaveBeenCalled();
  });

  it("selects a file on a single click without opening it", async () => {
    render(<PromptsSkillsCard />);
    await screen.findByText("prompts");

    fireEvent.click(screen.getByText("skills"));
    fireEvent.click(await screen.findByText("greeting"));
    const file = await screen.findByText("SKILL.md");
    fireEvent.click(file);

    expect(h.openFile).not.toHaveBeenCalled();
    // Selected row carries the inset selected fill.
    expect(file.closest("button")?.className).toContain(SELECTED_FILL);
  });

  it("opens a file on a double click with the package workspace id and the walked path", async () => {
    render(<PromptsSkillsCard />);
    await screen.findByText("prompts");

    fireEvent.click(screen.getByText("skills"));
    fireEvent.click(await screen.findByText("greeting"));
    fireEvent.dblClick(await screen.findByText("SKILL.md"));

    expect(h.openFile).toHaveBeenCalledWith("com.test.agent", "__agent_package__", "skills/greeting/SKILL.md");
  });

  it("moves the selection highlight between files", async () => {
    render(<PromptsSkillsCard />);
    fireEvent.click(await screen.findByText("skills"));
    fireEvent.click(await screen.findByText("greeting"));
    const skill = await screen.findByText("SKILL.md");
    fireEvent.click(skill);
    expect(skill.closest("button")?.className).toContain(SELECTED_FILL);

    fireEvent.click(screen.getByText("prompts"));
    const system = await screen.findByText("system.md");
    fireEvent.click(system);
    expect(system.closest("button")?.className).toContain(SELECTED_FILL);
    expect(skill.closest("button")?.className).not.toContain(SELECTED_FILL);
  });

  it("clears the selection when the directory holding it is collapsed", async () => {
    render(<PromptsSkillsCard />);
    fireEvent.click(await screen.findByText("skills"));
    fireEvent.click(await screen.findByText("greeting"));
    fireEvent.click(await screen.findByText("SKILL.md"));
    const skill = screen.getByText("SKILL.md");
    expect(skill.closest("button")?.className).toContain(SELECTED_FILL);

    // Collapse `skills/` — the selected row leaves the DOM, and the stale
    // highlight must not reappear when the directory is opened again.
    fireEvent.click(screen.getByText("skills"));
    fireEvent.click(screen.getByText("skills"));
    // Child rows unmount on collapse, so re-walk the same path.
    fireEvent.click(await screen.findByText("greeting"));
    const again = await screen.findByText("SKILL.md");
    expect(again.closest("button")?.className).not.toContain(SELECTED_FILL);
  });

  it("reloads through the prompts endpoint and then invalidates the tree cache", async () => {
    render(<PromptsSkillsCard />);
    await screen.findByText("prompts");

    await fireEvent.click(screen.getByRole("button", { name: "Reload prompts & skills" }));

    await waitFor(() => {
      const call = (globalThis.fetch as unknown as ReturnType<typeof vi.fn>).mock.calls[0];
      expect(call[0]).toBe("http://127.0.0.1:19876/api/agents/com.test.agent/prompts/reload");
      expect(call[1]).toMatchObject({ method: "POST" });
    });
    expect(h.invalidate).toHaveBeenCalledWith("com.test.agent");
  });

  it("shows the empty-package state instead of a blank body", async () => {
    h.fetchTreeNode.mockResolvedValue({ kind: "ready" as const, root: "C:/pkg", fetchedAt: Date.now(), entries: [] });
    render(<PromptsSkillsCard />);
    expect(await screen.findByText("No prompts/ or skills/ in this package")).toBeTruthy();
  });

  it("surfaces a tree load failure without rendering rows", async () => {
    h.fetchTreeNode.mockResolvedValue({ kind: "error" as const, error: { cause: "network" as const, message: "x" } });
    render(<PromptsSkillsCard />);
    expect(await screen.findByText("Failed to load prompts & skills")).toBeTruthy();
    expect(screen.queryByText("prompts")).toBeNull();
  });
});
