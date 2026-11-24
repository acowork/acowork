/**
 * Regression: clicking a `[file.ts:42](file.ts#L42)` link in chat must land on
 * line 42 the FIRST time.
 *
 * First open is the hard case: the panel mounts lazily and Monaco finishes its
 * bootstrap AFTER the file content arrived, so `editor.create()` binds the
 * target model directly. @monaco-editor/react only calls setModel() when the
 * `path` prop CHANGES, so onDidChangeModel never fires for that first bind —
 * a navigation queued behind the loading spinner used to be dropped and the
 * file opened at line 1 (a second click worked because the model was bound by
 * then). FileEditorPanel now drains pendingNavigationRef on editor mount.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, act } from "@testing-library/react";
import { useEffect, useRef } from "react";

// ── Hoisted mutable fixtures + spies ─────────────────────────────────────

const h = vi.hoisted(() => {
  const MockEditor = vi.fn(() => null);
  const MockDiffEditor = vi.fn(() => null);
  return {
    MockEditor,
    MockDiffEditor,
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
    chatState: { addAttachedContext: vi.fn(), getActiveSessionId: vi.fn(() => null) },
    agentState: { selectedAgentId: null as string | null },
    workspaceState: { sessionWorkspaceMap: {} as Record<string, string>, requestLocate: vi.fn() },
    layoutState: { requestShowWorkspacePanel: vi.fn() },
    settingsState: { theme: "light" as string, fontSize: 14, osTheme: "light" as string },
    gitState: { isExpanded: vi.fn(() => false) },
    editorStatusState: {
      cursor: { line: 1, column: 1 },
      selectedCount: 0,
      resetToIdle: vi.fn(),
      setCursor: vi.fn(),
      setSelectedCount: vi.fn(),
    },
    treeState: { getNode: vi.fn(() => null) },
  };
});

// ── Fake Monaco editor / model ───────────────────────────────────────────

const MODEL_PATH = "/src/a.ts";

function makeModel(lineCount: number) {
  return {
    uri: { scheme: "file", path: MODEL_PATH, toString: () => "file://" + MODEL_PATH },
    getLineCount: () => lineCount,
    getValue: () => "x\n".repeat(lineCount),
    getLanguageId: () => "typescript",
  };
}

function makeEditor(model: ReturnType<typeof makeModel>) {
  const calls: Record<string, unknown[][]> = {};
  const record = (name: string) => (...args: unknown[]) => {
    (calls[name] ??= []).push(args);
    return { dispose: () => {} };
  };
  const base: Record<string, unknown> = {
    getModel: () => model,
    saveViewState: () => null,
    restoreViewState: () => {},
    getLayoutInfo: () => ({ width: 800, height: 600 }),
    revealLineInCenter: record("revealLineInCenter"),
    setPosition: record("setPosition"),
    setSelection: record("setSelection"),
    revealRangeInCenter: record("revealRangeInCenter"),
    addCommand: record("addCommand"),
    addAction: record("addAction"),
  };
  const editor = new Proxy(base, {
    get: (target, key) =>
      key in target ? target[key as string] : record(String(key)),
  });
  return { editor, calls };
}

const model = makeModel(100);
const { editor: fakeEditor, calls: editorCalls } = makeEditor(model);

const fakeMonaco = {
  Uri: {
    parse: (s: string) => ({
      scheme: "file",
      path: "/" + s.replace(/^\/+/, ""),
      toString: () => "file:///" + s.replace(/^\/+/, ""),
    }),
  },
  KeyMod: new Proxy({}, { get: () => 1 }),
  KeyCode: new Proxy({}, { get: () => 1 }),
  KeyChord: new Proxy({}, { get: () => 1 }),
  editor: {
    getModel: () => model,
    getModels: () => [model],
    createModel: () => model,
  },
};

// ── Module mocks ─────────────────────────────────────────────────────────

vi.mock("@monaco-editor/react", () => ({
  default: (props: Record<string, unknown>) => {
    h.MockEditor(props);
    const mounted = useRef(false);
    useEffect(() => {
      if (mounted.current) return;
      mounted.current = true;
      (props.onMount as (e: unknown, m: unknown) => void)(fakeEditor, fakeMonaco);
    }, []);
    return null;
  },
  Editor: (props: Record<string, unknown>) => props,
  DiffEditor: (props: Record<string, unknown>) => {
    h.MockDiffEditor(props);
    return null;
  },
}));

vi.mock("../../lib/monacoBootstrap", () => ({
  initMonaco: () => Promise.resolve(),
}));

vi.mock("./lspProviders", () => ({
  registerLspProviders: () => () => {},
  disposeModelForFile: () => {},
  unpinPreviewModel: () => {},
}));

vi.mock("../markdown/editorAid", () => ({
  registerMarkdownTableNavigation: () => () => {},
}));

vi.mock("../../hooks/useLspClientPool", () => ({
  useLspClientPool: () => ({ activeStatus: null, activeStatusMessage: "", activeClient: null }),
}));

function selectorStore<T>(state: T) {
  return Object.assign(
    (selector: (s: T) => unknown) => selector(state),
    {
      getState: () => state,
      setState: (updater: (s: T) => Partial<T>) => {
        Object.assign(state as object, updater(state));
      },
    },
  );
}

vi.mock("../../stores/fileEditorStore", () => ({
  registerFileDisposer: () => () => {},
  useFileEditorStore: selectorStore(h.editorState),
  sourceRelPath: (file: { relPath: string }) => file.relPath,
}));

vi.mock("../../stores/chatStore", () => ({ useChatStore: selectorStore(h.chatState) }));
vi.mock("../../stores/agentStore", () => ({ useAgentStore: selectorStore(h.agentState) }));
vi.mock("../../stores/workspaceStore", () => ({ useWorkspaceStore: selectorStore(h.workspaceState) }));
vi.mock("../../stores/layoutStore", () => ({ useLayoutStore: selectorStore(h.layoutState) }));
vi.mock("../../stores/settingsStore", () => ({ useSettingsStore: selectorStore(h.settingsState) }));
vi.mock("../../stores/editorStatusStore", () => ({
  // The status footer subscribes to this store as a hook (cursor +
  // selection), so the mock must be callable with a selector — the same
  // shape `selectorStore` gives the other stores.
  useEditorStatusStore: selectorStore(h.editorStatusState),
}));
vi.mock("../../stores/fileTree", () => ({
  useFileTreeStore: { getState: () => h.treeState },
  treeKey: (a: string, w: string, p: string) => `${a}\u0000${w}\u0000${p}`,
  isReadyNode: () => false,
}));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../lib/logger", () => ({
  log: { trace: () => {}, debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));
vi.mock("../../lib/config", () => ({
  getGatewayUrl: () => "http://gw.test",
  DEFAULT_GATEWAY_URL: "http://gw.test",
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

import { FileEditorPanel } from "./FileEditorPanel";

function sourceFile(overrides: Record<string, unknown> = {}) {
  return {
    id: "file:ws1:src/a.ts",
    agentId: "a1",
    workspaceId: "ws1",
    relPath: "src/a.ts",
    fileName: "a.ts",
    content: "x\n".repeat(100),
    originalContent: "x\n".repeat(100),
    loading: false,
    saving: false,
    language: "typescript",
    dirty: false,
    mode: "edit",
    kind: "file",
    ...overrides,
  };
}

beforeEach(() => {
  h.editorState.openFiles = [];
  h.editorState.activeFileId = null;
  h.MockEditor.mockClear();
  h.MockDiffEditor.mockClear();
  for (const key of Object.keys(editorCalls)) delete editorCalls[key];
  // Run rAF inline so the deferred jump is observable in the same tick.
  vi.stubGlobal("requestAnimationFrame", (cb: (t: number) => void) => {
    cb(0);
    return 0;
  });
});

describe("FileEditorPanel line jump on first open", () => {
  it("jumps to cursorLine when Monaco binds the model at editor creation", async () => {
    // The reported case: file opened from a chat link carrying `#L42`.
    // cursorLine is set while the panel is still mounting, so the jump is
    // queued — and the editor is then created with the model already bound,
    // which never fires onDidChangeModel.
    const file = sourceFile({ cursorLine: 42 });
    h.editorState.openFiles = [file];
    h.editorState.activeFileId = file.id;

    render(<FileEditorPanel width={800} />);

    await vi.waitFor(() => {
      expect(h.MockEditor).toHaveBeenCalled();
    });
    await act(async () => {}); // flush monacoReady + onMount

    expect(editorCalls.setPosition?.at(-1)).toEqual([{ lineNumber: 42, column: 1 }]);
    expect(editorCalls.revealLineInCenter?.at(-1)).toEqual([42]);
  });

  it("clamps a stale line anchor to the model's real length", async () => {
    const file = sourceFile({ cursorLine: 999 });
    h.editorState.openFiles = [file];
    h.editorState.activeFileId = file.id;

    render(<FileEditorPanel width={800} />);

    await vi.waitFor(() => {
      expect(h.MockEditor).toHaveBeenCalled();
    });
    await act(async () => {});

    expect(editorCalls.setPosition?.at(-1)).toEqual([{ lineNumber: 100, column: 1 }]);
  });

  it("does not jump when the file has no cursorLine", async () => {
    const file = sourceFile();
    h.editorState.openFiles = [file];
    h.editorState.activeFileId = file.id;

    render(<FileEditorPanel width={800} />);

    await vi.waitFor(() => {
      expect(h.MockEditor).toHaveBeenCalled();
    });
    await act(async () => {});

    expect(editorCalls.setPosition).toBeUndefined();
    expect(editorCalls.revealLineInCenter).toBeUndefined();
  });
});