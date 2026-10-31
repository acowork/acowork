/**
 * FileEditorPanel — image preview branch.
 *
 * The viewer itself is covered by `ImageViewer.test.tsx`; what this file pins
 * is the WIRING between the panel and it, because every part of it fails
 * silently rather than loudly:
 *
 *   1. a raster image in preview mode reaches the viewer with the base64
 *      `data:` URL the Gateway returned (and NOT a broken empty one while the
 *      bytes are still loading) — and with no double-click-to-source
 *      affordance, since a raster image's "source" is a base64 blob;
 *   2. an SVG reaches it the same way (re-encoded from its XML text, which is
 *      what keeps `#` / `<` / `>` out of a utf8 data URI) and DOES offer the
 *      source toggle;
 *   3. the viewer's two chat gestures become a composer insertion carrying the
 *      workspace-relative path the agent's tools resolve — the panel builds
 *      that text, not the viewer;
 *   4. when no composer can take the reference, the viewer is told why through
 *      `addToChatDisabledTitle` instead of being handed a dead callback.
 *
 * Everything heavy (Monaco, LSP, the stores) is stubbed, same as
 * `FileEditorPanel.diffLog.test.tsx`.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen } from "@testing-library/react";

const h = vi.hoisted(() => ({
  viewerProps: [] as Array<Record<string, unknown>>,
  insertText: vi.fn(),
  attachImage: vi.fn(),
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
  // The key alone would let a wrong-argument bug through; appending the
  // interpolation params is what makes the assertions below meaningful.
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
// The two composer gestures the panel emits — asserted directly.
vi.mock("../../lib/chatComposerBridge", () => ({
  insertIntoChatComposer: h.insertText,
  attachImageToChatComposer: h.attachImage,
}));

// Children we are not testing → null stubs. `ImageViewer` records its props.
vi.mock("./ImageViewer", () => ({
  ImageViewer: (props: Record<string, unknown>) => {
    h.viewerProps.push(props);
    return <div data-testid="image-viewer" />;
  },
}));
vi.mock("./LspDocumentTracker", () => ({ LspDocumentTracker: () => null }));
vi.mock("./MarkdownPreviewView", () => ({ MarkdownPreviewView: () => null }));
vi.mock("./UrlPreviewView", () => ({ UrlPreviewView: () => null }));
vi.mock("./HtmlPreviewView", () => ({ HtmlPreviewView: () => null }));
vi.mock("./GoToFilePalette", () => ({ GoToFilePalette: () => null }));
vi.mock("./SymbolSearchPanel", () => ({ SymbolSearchPanel: () => null }));
vi.mock("../common/ScrollableTabBar", () => ({ ScrollableTabBar: () => null }));
vi.mock("../common/tab", () => ({ TabItem: () => null }));
vi.mock("../common/SetiIcon", () => ({ SetiIcon: () => null }));
vi.mock("../common/Tooltip", () => ({ Tooltip: () => null }));
vi.mock("../workspace/FileTree/fileIcons", () => ({ getFileIcon: () => null }));

import { FileEditorPanel } from "./FileEditorPanel";

function imageFile(overrides: Record<string, unknown> = {}) {
  return {
    id: "a1:ws1:docs/logo.png",
    agentId: "a1",
    workspaceId: "ws1",
    relPath: "docs/logo.png",
    fileName: "logo.png",
    content: "QUJD",
    originalContent: "QUJD",
    loading: false,
    saving: false,
    language: "plaintext",
    dirty: false,
    mode: "preview",
    kind: "file",
    mimeType: "image/png",
    ...overrides,
  };
}

function renderWith(file: Record<string, unknown>) {
  h.editorState.openFiles = [file];
  h.editorState.activeFileId = file.id as string;
  render(<FileEditorPanel width={800} />);
}

const latestViewerProps = () =>
  h.viewerProps.at(-1) as {
    src: string;
    fileName: string;
    onOpenSource?: () => void;
    addToChatDisabledTitle: string | null;
    onAddPointToChat: (point: { x: number; y: number }) => void;
    onAddRegionToChat: (file: File, region: { x: number; y: number; width: number; height: number }) => void;
  };

beforeEach(() => {
  h.viewerProps.length = 0;
  h.agentState.selectedAgentId = null;
  h.insertText.mockClear();
  h.attachImage.mockClear();
  h.editorState.openFiles = [];
  h.editorState.activeFileId = null;
});

describe("FileEditorPanel image preview", () => {
  it("hands the viewer the image as a base64 data URL, without a source toggle", () => {
    renderWith(imageFile());
    expect(screen.getByTestId("image-viewer")).toBeTruthy();
    const props = latestViewerProps();
    expect(props.src).toBe("data:image/png;base64,QUJD");
    expect(props.fileName).toBe("logo.png");
    // A raster image's source is a base64 blob — no double-click-to-edit.
    expect(props.onOpenSource).toBeUndefined();
  });

  it("re-encodes SVG text and offers the source toggle", () => {
    renderWith(
      imageFile({
        id: "a1:ws1:docs/icon.svg",
        relPath: "docs/icon.svg",
        fileName: "icon.svg",
        content: '<svg xmlns="http://www.w3.org/2000/svg" />',
        mimeType: "image/svg+xml",
      }),
    );
    const props = latestViewerProps();
    expect(props.src.startsWith("data:image/svg+xml;base64,")).toBe(true);
    expect(atob(props.src.split(",")[1])).toBe('<svg xmlns="http://www.w3.org/2000/svg" />');
    expect(typeof props.onOpenSource).toBe("function");
  });

  it("does not mount the viewer until the bytes have arrived", () => {
    renderWith(imageFile({ loading: true }));
    expect(screen.queryByTestId("image-viewer")).toBeNull();
  });

  it("turns a coordinate into a composer insertion naming the file path", () => {
    renderWith(imageFile());
    latestViewerProps().onAddPointToChat({ x: 12, y: 34 });
    // The panel formats with the workspace-relative path, not the basename —
    // that is what the agent's own tools resolve.
    expect(h.insertText).toHaveBeenCalledWith(
      'fileEditor.imageViewer.pointPrompt {"path":"docs/logo.png","x":12,"y":34}',
    );
  });

  it("attaches the cropped region and says where it came from", () => {
    renderWith(imageFile());
    const file = new File(["png"], "logo-region-10x10.png", { type: "image/png" });
    latestViewerProps().onAddRegionToChat(file, { x: 5, y: 6, width: 10, height: 10 });
    expect(h.attachImage).toHaveBeenCalledWith(file);
    // Inclusive far corner + size, so the text describes the same pixels the
    // crop contains.
    expect(h.insertText).toHaveBeenCalledWith(
      'fileEditor.imageViewer.regionPrompt {"path":"docs/logo.png","x":5,"y":6,"x2":14,"y2":15,"width":10,"height":10}',
    );
  });

  it("explains why the chat items are disabled when no composer can take them", () => {
    // The file belongs to the selected agent, but no session resolved in this
    // harness → the reference has nowhere to land.
    h.agentState.selectedAgentId = "a1";
    renderWith(imageFile());
    expect(latestViewerProps().addToChatDisabledTitle).toBe(
      "workspace.search.addToChatDisabled.no-session",
    );
  });

  it("refuses to attach a file from another agent, and says so", () => {
    h.agentState.selectedAgentId = "someone-else";
    renderWith(imageFile());
    expect(latestViewerProps().addToChatDisabledTitle).toBe(
      "fileEditor.imageViewer.unavailable",
    );
  });
});