/**
 * FileEditorPanel — document preview branch wiring.
 *
 * Pins the contract of the remote-file-preview plan at the panel level:
 *
 *   1. document tabs (pdf / docx / pptx / xlsx / xls, plus legacy doc/ppt
 *      and csv *in preview mode*) are intercepted by the document branch
 *      and route to their lazy viewer — never to Monaco / MarkdownPreview /
 *      the image viewer, which would render the deliberately-empty
 *      `content` buffer;
 *   1b. every Office format (including the legacy pair) reaches the
 *      PDF-conversion wrapper with its previous renderer passed as the
 *      `fallback` — the conversion is an upgrade, never a dead end; PDF
 *      and CSV tabs are untouched by it;
 *   2. document tabs carry no Monaco path at all: .pdf in EDIT mode still
 *      reaches the viewer (DOC_SOURCE_HIDDEN_RE), while csv in edit mode
 *      stays in Monaco (its content is real text);
 *   3. the viewer receives the five-prop `ViewerFileProps` triple-plus-
 *      meta (agentId / workspaceId / relPath / fileName / previewSeq) so
 *      it can fetch bytes itself and react to fs-watch bumps.
 *
 * Everything heavy is stubbed exactly as in
 * `FileEditorPanel.imagePreview.test.tsx`.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import type { ReactElement } from "react";

const h = vi.hoisted(() => ({
  viewerProps: [] as Array<{ testid: string; props: Record<string, unknown> }>,
  editorState: {
    openFiles: [] as Array<Record<string, unknown>>,
    activeFileId: null as string | null,
    setActiveFile: vi.fn(),
    updateContent: vi.fn(),
    saveFile: vi.fn(),
    closeFile: vi.fn(),
    closeOthers: vi.fn(),
    closeAllFiles: vi.fn(),
    refreshFile: vi.fn(),
    openFile: vi.fn(),
    openPreview: vi.fn(),
  },
  chatState: {
    addAttachedContext: vi.fn(),
    getActiveSessionId: vi.fn(() => null),
    agentStates: {} as Record<string, unknown>,
  },
  agentState: { selectedAgentId: null as string | null, agents: {} as Record<string, unknown> },
  workspaceState: { sessionWorkspaceMap: {} as Record<string, string>, requestLocate: vi.fn() },
  layoutState: { requestShowWorkspacePanel: vi.fn() },
  settingsState: { theme: "light" as string, fontSize: 14, osTheme: "light" as string },
  gitState: { isExpanded: vi.fn(() => false) },
  editorStatusState: {
    setLspSignals: vi.fn(),
    resetToIdle: vi.fn(),
    setCursor: vi.fn(),
    setSelectedCount: vi.fn(),
  },
  treeState: { getNode: vi.fn(() => null) },
  blobHook: vi.fn(),
}));

vi.mock("@monaco-editor/react", () => ({
  default: () => null,
  Editor: () => null,
  DiffEditor: () => null,
}));

vi.mock("../../lib/monacoBootstrap", () => ({ initMonaco: () => Promise.resolve() }));
vi.mock("./lspProviders", () => ({
  registerLspProviders: () => () => {},
  disposeModelForFile: () => {},
  unpinPreviewModel: () => {},
}));
vi.mock("../../hooks/useLspClientPool", () => ({
  useLspClientPool: () => ({ activeStatus: null, activeStatusMessage: "", activeClient: null }),
}));
vi.mock("../../hooks/useReportFilePanelBounds", () => ({ useReportFilePanelBounds: () => {} }));

function selectorStore<T>(state: T) {
  return Object.assign((selector: (s: T) => unknown) => selector(state), {
    getState: () => state,
  });
}

vi.mock("../../stores/fileEditorStore", () => ({
  registerFileDisposer: () => () => {},
  useFileEditorStore: selectorStore(h.editorState),
  sourceRelPath: (file: { kind: string; relPath: string }) =>
    file.kind === "diff" || file.kind === "log"
      ? file.relPath.replace(/^(diff|log):/, "")
      : file.relPath,
}));
vi.mock("../../stores/chatStore", () => ({ useChatStore: selectorStore(h.chatState) }));
vi.mock("../../stores/agentStore", () => ({ useAgentStore: selectorStore(h.agentState) }));
vi.mock("../../stores/workspaceStore", () => ({ useWorkspaceStore: selectorStore(h.workspaceState) }));
vi.mock("../../stores/layoutStore", () => ({ useLayoutStore: selectorStore(h.layoutState) }));
vi.mock("../../stores/settingsStore", () => ({ useSettingsStore: selectorStore(h.settingsState) }));
vi.mock("../../stores/editorStatusStore", () => ({
  useEditorStatusStore: { getState: () => h.editorStatusState },
}));
vi.mock("../../stores/fileTree", () => ({
  useFileTreeStore: { getState: () => h.treeState },
  treeKey: (a: string, w: string, p: string) => `${a}\u0000${w}\u0000${p}`,
  isReadyNode: () => false,
  getCachedWorkspaceRoot: () => "/root/ws",
}));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, unknown>) =>
      params ? `${key} ${JSON.stringify(params)}` : key,
  }),
}));
vi.mock("../../lib/logger", () => ({
  log: { trace: () => {}, debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));
vi.mock("../../lib/config", () => ({
  getGatewayUrl: () => "http://gw.test",
  DEFAULT_GATEWAY_URL: "http://gw.test",
}));
vi.mock("../../lib/chatComposerBridge", () => ({
  insertIntoChatComposer: vi.fn(),
  attachImageToChatComposer: vi.fn(),
}));

vi.mock("./ImageViewer", () => ({ ImageViewer: () => null }));
// The blob hook is stubbed (the image branch calls it unconditionally at
// top level); document branches fetch inside their own viewers.
vi.mock("../../hooks/useWorkspaceFileBlob", () => ({
  useWorkspaceFileBlob: h.blobHook,
}));
vi.mock("./LspDocumentTracker", () => ({ LspDocumentTracker: () => null }));
vi.mock("./MarkdownPreviewView", () => ({ MarkdownPreviewView: () => null }));
vi.mock("./UrlPreviewView", () => ({ UrlPreviewView: () => null }));
vi.mock("./WorkspaceHtmlPreview", () => ({ WorkspaceHtmlPreview: () => null }));
vi.mock("./GoToFilePalette", () => ({ GoToFilePalette: () => null }));
vi.mock("./SymbolSearchPanel", () => ({ SymbolSearchPanel: () => null }));
vi.mock("../common/ScrollableTabBar", () => ({ ScrollableTabBar: () => null }));
vi.mock("../common/tab", () => ({ TabItem: () => null }));
vi.mock("../common/SetiIcon", () => ({ SetiIcon: () => null }));
vi.mock("../common/Tooltip", () => ({ Tooltip: () => null }));
vi.mock("../workspace/FileTree/fileIcons", () => ({ getFileIcon: () => null }));

// The lazy document viewers → recording stubs. `vi.mock` also intercepts
// the dynamic `import()` behind `React.lazy`, so the panel resolves to
// these instead of the real (parser-loading) components.
vi.mock("./PdfViewer", () => ({
  default: (props: Record<string, unknown>) => {
    h.viewerProps.push({ testid: "pdf-viewer", props });
    return <div data-testid="pdf-viewer" />;
  },
}));
vi.mock("./SheetViewer", () => ({
  default: (props: Record<string, unknown>) => {
    h.viewerProps.push({ testid: "sheet-viewer", props });
    return <div data-testid="sheet-viewer" />;
  },
}));
vi.mock("./DocxViewer", () => ({
  default: (props: Record<string, unknown>) => {
    h.viewerProps.push({ testid: "docx-viewer", props });
    return <div data-testid="docx-viewer" />;
  },
}));
vi.mock("./PptxViewer", () => ({
  default: (props: Record<string, unknown>) => {
    h.viewerProps.push({ testid: "pptx-viewer", props });
    return <div data-testid="pptx-viewer" />;
  },
}));
vi.mock("./UnsupportedDocViewer", () => ({
  default: (props: Record<string, unknown>) => {
    h.viewerProps.push({ testid: "unsupported-viewer", props });
    return <div data-testid="unsupported-viewer" />;
  },
}));
// The Office→PDF wrapper renders its `fallback` prop when it reports the
// degraded states, so mounting it here also mounts (and records) the
// viewer the tab would fall back to — which is the wiring under test.
vi.mock("./OfficePdfViewer", () => ({
  default: (props: Record<string, unknown>) => {
    h.viewerProps.push({ testid: "office-pdf-viewer", props });
    return <div data-testid="office-pdf-viewer">{props.fallback as ReactElement}</div>;
  },
}));

import { FileEditorPanel } from "./FileEditorPanel";

/** A loaded document tab: `content` EMPTY by design (the store skips the
 *  base64 JSON read for these), exactly as the real tabs arrive. */
function docFile(name: string, relBase: string, overrides: Record<string, unknown> = {}) {
  return {
    id: `a1:ws1:${relBase}`,
    agentId: "a1",
    workspaceId: "ws1",
    relPath: relBase,
    fileName: name,
    content: "",
    originalContent: "",
    loading: false,
    saving: false,
    language: "plaintext",
    dirty: false,
    mode: "preview",
    kind: "file",
    mimeType: "application/octet-stream",
    previewSeq: 3,
    ...overrides,
  };
}

function renderWith(file: Record<string, unknown>) {
  h.editorState.openFiles = [file];
  h.editorState.activeFileId = file.id as string;
  render(<FileEditorPanel width={800} />);
}

const latest = () => h.viewerProps.at(-1) as { testid: string; props: Record<string, unknown> };

beforeEach(() => {
  cleanup();
  h.viewerProps.length = 0;
  h.agentState.selectedAgentId = null;
  h.editorState.openFiles = [];
  h.editorState.activeFileId = null;
  h.blobHook.mockReset();
  h.blobHook.mockImplementation(() => ({
    blob: null,
    blobUrl: null,
    mime: null,
    loading: false,
    error: null,
    retry: vi.fn(),
  }));
});

describe("FileEditorPanel document preview branch", () => {
  it("routes a PDF tab to the lazy PdfViewer with the viewer props", async () => {
    renderWith(docFile("report.pdf", "docs/report.pdf", { mimeType: "application/pdf" }));
    const el = await screen.findByTestId("pdf-viewer");
    expect(el).toBeTruthy();
    expect(latest().props).toMatchObject({
      agentId: "a1",
      workspaceId: "ws1",
      relPath: "docs/report.pdf",
      fileName: "report.pdf",
      previewSeq: 3,
    });
    // No empty-buffer fallbacks were consulted (image / sheet viewers).
    expect(screen.queryByTestId("image-viewer")).toBeNull();
    expect(screen.queryByTestId("sheet-viewer")).toBeNull();
  });

  it("hides the Monaco path from document tabs: a .pdf in EDIT mode still reaches the viewer", async () => {
    renderWith(docFile("report.pdf", "docs/report.pdf", { mode: "edit" }));
    expect(await screen.findByTestId("pdf-viewer")).toBeTruthy();
  });

  it("routes xlsx and xls through the PDF wrapper, falling back to the sheet viewer", async () => {
    renderWith(docFile("budget.xlsx", "docs/budget.xlsx", { previewSize: 2048, previewMtimeMs: 99 }));
    expect(await screen.findByTestId("office-pdf-viewer")).toBeTruthy();
    // `latest()` would be the fallback the wrapper mounted right after.
    const office = h.viewerProps.find((v) => v.testid === "office-pdf-viewer")!;
    expect(office.props).toMatchObject({
      relPath: "docs/budget.xlsx",
      fileName: "budget.xlsx",
      previewSeq: 3,
      // The version pair the converter's cache key is built from.
      size: 2048,
      mtimeMs: 99,
    });
    // The JS renderer is still handed over as the degraded fallback.
    expect(screen.getByTestId("sheet-viewer")).toBeTruthy();
    cleanup();
    h.viewerProps.length = 0;
    renderWith(docFile("legacy.xls", "docs/legacy.xls", { mode: "edit" }));
    expect(await screen.findByTestId("office-pdf-viewer")).toBeTruthy();
    expect(screen.getByTestId("sheet-viewer")).toBeTruthy();
  });

  it("routes csv to the sheet viewer in preview mode only — edit mode stays Monaco", async () => {
    renderWith(docFile("data.csv", "docs/data.csv"));
    expect(await screen.findByTestId("sheet-viewer")).toBeTruthy();
    h.viewerProps.length = 0;
    renderWith(
      docFile("data.csv", "docs/data2.csv", {
        id: "a1:ws1:docs/data2.csv",
        mode: "edit",
        content: "a,b\n1,2",
      }),
    );
    // Let any lazy resolution settle, then assert no viewer mounted.
    await new Promise((r) => setTimeout(r, 0));
    expect(h.viewerProps).toHaveLength(0);
  });

  it("routes docx and pptx through the PDF wrapper with their own fallback", async () => {
    renderWith(docFile("paper.docx", "docs/paper.docx"));
    expect(await screen.findByTestId("office-pdf-viewer")).toBeTruthy();
    expect(screen.getByTestId("docx-viewer")).toBeTruthy();
    h.viewerProps.length = 0;
    renderWith(docFile("deck.pptx", "docs/deck.pptx"));
    expect(await screen.findByTestId("office-pdf-viewer")).toBeTruthy();
    expect(screen.getByTestId("pptx-viewer")).toBeTruthy();
  });

  it("routes legacy .doc / .ppt through the wrapper too, with the card as fallback", async () => {
    // The engine converts binary formats natively, so these gain an
    // in-app preview; the card is only what a bare machine keeps showing.
    renderWith(docFile("old.doc", "docs/old.doc"));
    expect(await screen.findByTestId("office-pdf-viewer")).toBeTruthy();
    expect(screen.getByTestId("unsupported-viewer")).toBeTruthy();
    cleanup();
    h.viewerProps.length = 0;
    renderWith(docFile("old.ppt", "docs/old.ppt"));
    expect(await screen.findByTestId("office-pdf-viewer")).toBeTruthy();
    expect(screen.getByTestId("unsupported-viewer")).toBeTruthy();
  });

  it("leaves a plain PDF tab on the native viewer (no conversion involved)", async () => {
    renderWith(docFile("report.pdf", "docs/report.pdf", { mimeType: "application/pdf" }));
    expect(await screen.findByTestId("pdf-viewer")).toBeTruthy();
    expect(screen.queryByTestId("office-pdf-viewer")).toBeNull();
  });
});
