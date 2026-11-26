/**
 * Self-check for `DocImportTargetDialog` — 「添加到文档库」的目标选择器.
 *
 * Why it exists: this is the only place that decides WHERE an imported file
 * lands, and getting it wrong is silent (the file just shows up somewhere the
 * user did not mean). Three rules are pinned:
 *
 * - opening the dialog preselects the first library AND its root, so the
 *   primary action works without any digging;
 * - switching library in the left list moves the target to that library's
 *   root (a stale target from the previous library would import across
 *   libraries);
 * - picking a subdirectory — reachable by expanding — is what `onConfirm`
 *   reports, and it is reachable only after the directory actually loads.
 *
 * `doc-api` is mocked at the leaf (no network); the directory tree is served
 * one level at a time exactly like the service does.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

const mocks = vi.hoisted(() => ({
  listLibraries: vi.fn(),
  getTree: vi.fn(),
}));

vi.mock("../../lib/doc-api", () => ({
  listLibraries: mocks.listLibraries,
  getTree: mocks.getTree,
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

import { DocImportTargetDialog } from "./DocImportTargetDialog";

const LIBS = [
  { dir_id: "lib-a", name: "Alpha" },
  { dir_id: "lib-b", name: "Beta" },
] as never;

const TREES: Record<string, unknown> = {
  "lib-a": { dirs: [{ dir_id: "a-sub", name: "AlphaSub", deleted: false }], files: [] },
  "lib-b": { dirs: [{ dir_id: "b-sub", name: "BetaSub", deleted: false }], files: [] },
  "a-sub": { dirs: [{ dir_id: "a-deep", name: "AlphaDeep", deleted: false }], files: [] },
  "b-sub": { dirs: [], files: [] },
  "a-deep": { dirs: [], files: [] },
};

beforeEach(() => {
  mocks.listLibraries.mockReset().mockResolvedValue(LIBS);
  mocks.getTree.mockReset().mockImplementation(async (dirId: string) => TREES[dirId]);
});

describe("DocImportTargetDialog", () => {
  it("preselects the first library and its root", async () => {
    const onConfirm = vi.fn();
    render(<DocImportTargetDialog onCancel={() => {}} onConfirm={onConfirm} />);
    // 左列表（title=库名）与右侧「库根目录」行都会出现库名，按左列表定位。
    await screen.findByTitle("Alpha");
    fireEvent.click(screen.getByText("doc.importTarget.confirm"));
    expect(onConfirm).toHaveBeenCalledWith("lib-a");
  });

  it("moves the target to the newly picked library's root", async () => {
    const onConfirm = vi.fn();
    render(<DocImportTargetDialog onCancel={() => {}} onConfirm={onConfirm} />);
    await screen.findByTitle("Beta");
    fireEvent.click(screen.getByTitle("Beta"));
    await waitFor(() => expect(mocks.getTree).toHaveBeenCalledWith("lib-b"));
    fireEvent.click(screen.getByText("doc.importTarget.confirm"));
    expect(onConfirm).toHaveBeenCalledWith("lib-b");
  });

  it("reports an expanded subdirectory, not the library", async () => {
    const onConfirm = vi.fn();
    render(<DocImportTargetDialog onCancel={() => {}} onConfirm={onConfirm} />);
    await screen.findByText("AlphaSub");
    fireEvent.click(screen.getByText("AlphaSub"));
    fireEvent.click(screen.getByText("doc.importTarget.confirm"));
    expect(onConfirm).toHaveBeenCalledWith("a-sub");
  });

  it("loads a directory's children only when it is expanded", async () => {
    render(<DocImportTargetDialog onCancel={() => {}} onConfirm={() => {}} />);
    await screen.findByText("AlphaSub");
    expect(mocks.getTree).not.toHaveBeenCalledWith("a-sub");
    fireEvent.click(screen.getAllByLabelText("doc.expandDir")[0]);
    await waitFor(() => expect(mocks.getTree).toHaveBeenCalledWith("a-sub"));
    expect(await screen.findByText("AlphaDeep")).toBeTruthy();
  });
});