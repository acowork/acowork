/**
 * Self-check for `DocFilePreview` — 文档库导入条目的预览分派。
 *
 * The requirement this pins is a *tie* between two surfaces: an imported
 * docx / xlsx / pptx / pdf must render the same way the workspace renders it,
 * which means the installed craft app (wordcraft / deckcraft / gridcraft /
 * pdfcraft) wins when it declares the extension, and the built-in viewer is
 * the fallback. Both halves are asserted here because a doc pane that ignored
 * the app would look "fine" — just different from the tab next to it.
 *
 * The viewers themselves are stubbed: what is under test is the decision, not
 * pdf.js.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen } from "@testing-library/react";

const h = vi.hoisted(() => ({
  listApps: vi.fn(),
}));

vi.mock("../../lib/apps-api", () => ({ listApps: h.listApps }));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../hooks/useWorkspaceFileBlob", () => ({
  useWorkspaceFileBlob: () => ({ blob: null, blobUrl: null, mime: null, loading: false, error: null, retry: vi.fn() }),
}));
// The app pane fetches its own bytes through AppFrame; only the fact that it was
// chosen (and what it was told) matters here.
vi.mock("../../components/editor/VisualAppPreview", () => ({
  VisualAppPreview: ({ file }: { file: Record<string, string> }) => (
    <div data-testid="visual-app">{`${file.agentId}|${file.workspaceId}|${file.relPath}`}</div>
  ),
}));
vi.mock("../../components/editor/documentViewer", () => ({
  renderDocumentViewer: () => <div data-testid="builtin-viewer" />,
}));

import { DocFilePreview } from "./DocFilePreview";

/** An installed visual app that declares `docx`. */
const WORDCRAFT = {
  id: "wordcraft",
  name: "WordCraft",
  enabled: true,
  canvas_kind: "visual",
  canvas_extensions: ["docx"],
  nav_order: 1,
};

const ENTRY = {
  docId: "doc-7",
  path: "研发/季度报告.docx",
  name: "季度报告",
  ext: "docx",
};

beforeEach(() => {
  h.listApps.mockReset();
});

describe("DocFilePreview", () => {
  it("hands a docx to the craft app that declares it — the workspace rule", async () => {
    h.listApps.mockResolvedValue([WORDCRAFT]);
    render(<DocFilePreview entry={ENTRY} />);
    const app = await screen.findByTestId("visual-app");
    // The doc entry reaches the app through the same triple the viewers use:
    // the `__doc__` sentinel address its own bytes.
    expect(app.textContent).toBe("__doc__|doc-7|季度报告.docx");
    expect(screen.queryByTestId("builtin-viewer")).toBeNull();
  });

  it("falls back to the built-in viewer when no app claims the format", async () => {
    h.listApps.mockResolvedValue([]);
    render(<DocFilePreview entry={ENTRY} />);
    expect(await screen.findByTestId("builtin-viewer")).toBeTruthy();
    expect(screen.queryByTestId("visual-app")).toBeNull();
  });

  it("ignores a disabled app, even with the extension declared", async () => {
    h.listApps.mockResolvedValue([{ ...WORDCRAFT, enabled: false }]);
    render(<DocFilePreview entry={ENTRY} />);
    expect(await screen.findByTestId("builtin-viewer")).toBeTruthy();
    expect(screen.queryByTestId("visual-app")).toBeNull();
  });
});