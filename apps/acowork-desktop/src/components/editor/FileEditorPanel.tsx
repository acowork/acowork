import { useState, useRef, useEffect, useCallback, useMemo } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useFileEditorStore, registerFileDisposer, sourceRelPath, type OpenFile } from "../../stores/fileEditorStore";
import { useGitStore } from "../../stores/gitStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useFileTreeStore, getCachedWorkspaceRoot, treeKey, isReadyNode } from "../../stores/fileTree";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { useLayoutStore } from "../../stores/layoutStore";
import { useEditorStatusStore } from "../../stores/editorStatusStore";
import { useLspClientPool } from "../../hooks/useLspClientPool";
import { useReportFilePanelBounds } from "../../hooks/useReportFilePanelBounds";
import { cn } from "../../lib/utils";
import { CAPSULE_PANE_CN } from "../common/capsule";
import { getGatewayUrl } from "../../lib/config";
import { X, Save, Loader2, FileText, MessageSquarePlus, Eye, Code2, Locate, RefreshCw, XSquare, Files, AlertCircle } from "lucide-react";
import Editor, { DiffEditor, type OnMount } from "@monaco-editor/react";
import { initMonaco } from "../../lib/monacoBootstrap";
import { ScrollableTabBar } from "../common/ScrollableTabBar";
import { TabItem } from "../common/tab";
import { SetiIcon } from "../common/SetiIcon";
import { getFileIcon } from "../workspace/FileTree/fileIcons";
import { registerLspProviders, disposeModelForFile, unpinPreviewModel } from "./lspProviders";
import { LspDocumentTracker } from "./LspDocumentTracker";
import { WORKSPACE_PREVIEW_EXT_RE } from "./filePreview";
import {
  ContextMenu,
  useContextMenu,
  type ContextMenuItem,
} from "../common/ContextMenu";
import { MarkdownPreviewView } from "./MarkdownPreviewView";
import { UrlPreviewView } from "./UrlPreviewView";
import { HtmlPreviewView } from "./HtmlPreviewView";
import { GitVirtualNav } from "./GitVirtualNav";
import { CommitPicker } from "./CommitPicker";
import { MarkdownToolbar } from "../markdown/MarkdownToolbar";
import { registerMarkdownTableNavigation } from "../markdown/editorAid";
import type { IDisposable } from "monaco-editor";
import { GoToFilePalette } from "./GoToFilePalette";
import { SymbolSearchPanel } from "./SymbolSearchPanel";
import { useSearchStore } from "../../stores/searchStore";
import { Tooltip } from "../common/Tooltip";
import { log } from "../../lib/logger";
import { useActiveSessionReadOnly } from "../../lib/session-write-access";

/**
 * Encode a UTF-8 text string to base64 in a way that survives non-ASCII
 * characters (the standard `btoa` only accepts code points 0–255).
 *
 * Used by the SVG preview branch to wrap raw XML markup (delivered by the
 * gateway as plain text — SVG is no longer in BINARY_EXTENSIONS) into a
 * base64 data URI. Encoding to base64 avoids the fragility of percent-
 * encoding `#`, `<`, `>` and other SVG/XML-meaningful characters in a
 * `data:image/svg+xml;utf8,...` URI.
 *
 * Uses an explicit loop instead of `String.fromCharCode(...bytes)` because
 * spread blows the call stack for inputs above ~120 KB (V8 limit ~65 535
 * args). A plain `for` loop handles arbitrarily large SVGs.
 */
function encodeTextToBase64(text: string): string {
    const bytes = new TextEncoder().encode(text);
    let binary = "";
    for (let i = 0; i < bytes.length; i++) {
        binary += String.fromCharCode(bytes[i]);
    }
    return btoa(binary);
}

/**
 * Files that support BOTH source editing and rendered preview.
 * Keep in sync with the workspace-panel entry points (file-tree
 * double-click, search select, right-click "Preview" item) — they share
 * the same regex via `./filePreview.WORKSPACE_PREVIEW_EXT_RE` so this
 * rule can't drift out of sync. Raster images (png/jpg/gif/webp) are
 * intentionally excluded — their "source" is just a base64 blob, so
 * toggling to a source pane adds no value.
 */
function supportsPreviewToggle(file: OpenFile): boolean {
    return file.kind === "file" && WORKSPACE_PREVIEW_EXT_RE.test(file.fileName);
}

export function FileEditorPanel({ width }: { width: number }) {
    const { t } = useTranslation();

    // ── Layout bounds reporting (PR-1 of the unified status-bar refactor) ──
    // Reports the panel's `getBoundingClientRect()` to `useLayoutStore.filePanelBounds`
    // on every layout change. Lets the global status bar in `AppLayout` anchor
    // file-status items (language / LSP / cursor) to the panel's left & right
    // edges without any prop drilling or DOM cross-measurement. The hook is
    // rAF-coalesced so dragging the resize handle does not flood the store.
    const rootRef = useRef<HTMLDivElement | null>(null);
    useReportFilePanelBounds(rootRef);

    const openFiles = useFileEditorStore((s) => s.openFiles);
    const activeFileId = useFileEditorStore((s) => s.activeFileId);
    const setActiveFile = useFileEditorStore((s) => s.setActiveFile);
    const updateContent = useFileEditorStore((s) => s.updateContent);
    const saveFile = useFileEditorStore((s) => s.saveFile);
    const closeFile = useFileEditorStore((s) => s.closeFile);
    const closeOthers = useFileEditorStore((s) => s.closeOthers);
    const closeAllFiles = useFileEditorStore((s) => s.closeAllFiles);
    const refreshFile = useFileEditorStore((s) => s.refreshFile);
    const openFile = useFileEditorStore((s) => s.openFile);
    const openPreview = useFileEditorStore((s) => s.openPreview);
    const addAttachedContext = useChatStore((s) => s.addAttachedContext);
    const getActiveSessionId = useChatStore((s) => s.getActiveSessionId);
    const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
    const sessionWorkspaceMap = useWorkspaceStore((s) => s.sessionWorkspaceMap);
    const requestLocate = useWorkspaceStore((s) => s.requestLocate);
    const requestShowWorkspacePanel = useLayoutStore((s) => s.requestShowWorkspacePanel);

    const theme = useSettingsStore((s) => s.theme);
    const fontSize = useSettingsStore((s) => s.fontSize);
    const [closingFileId, setClosingFileId] = useState<string | null>(null);
    const [diffEditor, setDiffEditor] = useState<
        import("monaco-editor").editor.IStandaloneDiffEditor | null
    >(null);
    // Reset the DiffEditor state when the active tab changes — Monaco's
    // onMount will repopulate it on the next render once the new
    // DiffEditor mounts. Without this, GitVirtualNav briefly holds a
    // dead IStandaloneDiffEditor reference for one render cycle when
    // switching between two diff tabs, and a click in that window
    // would call goToDiff() on a disposed instance.
    useEffect(() => {
        setDiffEditor(null);
    }, [activeFileId]);
    // Monaco is loaded in the background (lib/monacoBootstrap.ts) so first
    // paint does not wait for it; gate <Editor> on it being ready. A failed
    // load must not leave an eternal spinner with no trace — monacoBootstrap
    // logs the error and drops its cached rejection, so remounting this panel
    // (close the last tab, reopen a file) performs the retry.
    const [monacoReady, setMonacoReady] = useState(false);
    const [monacoFailed, setMonacoFailed] = useState(false);
    useEffect(() => {
        let cancelled = false;
        initMonaco().then(
            () => {
                if (!cancelled) setMonacoReady(true);
            },
            () => {
                if (!cancelled) setMonacoFailed(true);
            },
        );
        return () => {
            cancelled = true;
        };
    }, []);
    // Tab right-click context menu. Payload is the fileId of the tab that
    // was right-clicked — items read it from `useContextMenu().payload` to
    // decide which file to act on.
    const tabMenu = useContextMenu<{ fileId: string }>();
    // Batch close confirmation (Close Others / Close All with dirty files)
    const [batchCloseRequest, setBatchCloseRequest] = useState<
        { kind: "others" | "all"; fileIds: string[]; dirtyCount: number; keepFileId?: string } | null
    >(null);
    const [showGoToFile, setShowGoToFile] = useState(false);
    const [showSymbolSearch, setShowSymbolSearch] = useState(false);
    const editorRef = useRef<Parameters<OnMount>[0] | null>(null);
    const monacoRef = useRef<typeof import("monaco-editor") | null>(null);
    // DiffEditor instance — state (not just a ref) so GitVirtualNav
    // re-renders when Monaco finishes mounting and we can show the
    // up / down hunk-jump buttons. DiffEditor remounts when the user
    // switches diff tabs (the `original` / `modified` props change),
    // so we clear it to null on unmount via the onMount teardown.
    // (cursor / selectedCount moved to useEditorStatusStore — see Monaco
    //  selection handler below. The local useState was only used by the
    //  per-file status bar that PR-3 removed.)
    // Selection range for "Add to Chat" floating button
    const [selectionRange, setSelectionRange] = useState<{ startLine: number; endLine: number } | null>(null);
    const [addToChatPos, setAddToChatPos] = useState<{ top: number; left: number } | null>(null);
    const lspProvidersRef = useRef<IDisposable | null>(null);
    const documentTrackerRef = useRef<LspDocumentTracker | null>(null);
    // Track the previous lspClient to detect reconnections
    const prevLspClientRef = useRef<typeof lspClient>(null);
    // When Monaco's peek widget navigates to a different file, we store the
    // target position here and apply it inside onDidChangeModel — the single
    // authoritative entry point for cross-file navigation. The Editor is no
    // longer keyed on activeFile.id, so model switching is synchronous and
    // we no longer need a separate useEffect-based fallback.
    const pendingNavigationRef = useRef<{ line: number; column: number; endLineNumber?: number; endColumn?: number } | null>(null);
    // Per-model view state cache (cursor / scroll / selection / folding).
    // Without per-file Editor remounts, undo history and scroll position would
    // bleed across files; we save/restore view state on model boundaries.
    const viewStatesRef = useRef<Map<string, unknown>>(new Map());
    // Guard to prevent overriding ICodeEditorService.openCodeEditor more than once
    // (it's a shared singleton service, not per-editor).
    const codeEditorOverriddenRef = useRef(false);

    const activeFile = openFiles.find((f) => f.id === activeFileId) ?? null;

    // Diff banner commit pickers. The two banners ("HEAD" / "Working
    // Tree") on a diff tab are buttons - clicking opens this picker
    // anchored under the button. Only one side is open at a time.
    const [pickerSide, setPickerSide] = useState<"base" | "head" | null>(null);
    const [baseAnchor, setBaseAnchor] = useState<HTMLElement | null>(null);
    const [headAnchor, setHeadAnchor] = useState<HTMLElement | null>(null);

    const fetchDiff = useGitStore((s) => s.fetchDiff);
    const updateDiffRefs = useFileEditorStore((s) => s.updateDiffRefs);

    /** Apply a new base/head ref pair: re-fetch and replace the diff
     *  tab content in place (no remount, no flicker). The base/head
     *  refs on the OpenFile are updated so the banner labels render
     *  the new selection immediately. */
    const applyDiffRefs = useCallback(
        async (baseRef: string, headRef: string) => {
            if (!activeFile) return;
            try {
                const diff = await fetchDiff(
                    activeFile.agentId,
                    activeFile.workspaceId,
                    sourceRelPath(activeFile),
                    baseRef,
                    headRef,
                );
                updateDiffRefs(activeFile.id, baseRef, headRef);
                useFileEditorStore.getState().setVirtualFileContent(
                    activeFile.id,
                    diff.modified,
                );
                useFileEditorStore.setState((s) => ({
                    openFiles: s.openFiles.map((f) =>
                        f.id === activeFile.id
                            ? {
                                  ...f,
                                  originalContent: diff.original,
                                  // Re-render the placeholder when the new
                                  // pair degrades (binary) or has no
                                  // changes at all — the kind belongs to
                                  // this ref pair, not to the tab.
                                  gitDiffKind: diff.kind,
                              }
                            : f,
                    ),
                }));
            } catch (e) {
                console.error("[FileEditorPanel] applyDiffRefs failed:", e);
            }
        },
        [activeFile, fetchDiff, updateDiffRefs],
    );

    // ── Locate-in-tree eligibility ──────────────────────────────────
    // The button is only enabled when the active file lives in the currently
    // selected agent AND the currently active session's workspace. Otherwise
    // the workspace tree wouldn't actually contain this file (or it'd be the
    // wrong file), and revealing it would be misleading.
    const activeSessionId = selectedAgentId ? getActiveSessionId(selectedAgentId) : null;
    // ADR-076: gates the "Add to Chat" entry points (see the
    // `addToChatDisabledReason` reasons below).
    const readOnlySession = useActiveSessionReadOnly();
    const currentWorkspaceId = activeSessionId
        ? (sessionWorkspaceMap[activeSessionId] ?? "__agent_home__")
        : null;

    type LocateDisabledReason = "no-file" | "loading" | "url-preview" | "wrong-agent" | "wrong-workspace" | null;
    const locateDisabledReason: LocateDisabledReason = (() => {
        if (!activeFile) return "no-file";
        if (activeFile.loading) return "loading";
        if (activeFile.kind === "url") return "url-preview";
        if (activeFile.agentId !== selectedAgentId) return "wrong-agent";
        if (currentWorkspaceId !== null && activeFile.workspaceId !== currentWorkspaceId) return "wrong-workspace";
        return null;
    })();
    const locateDisabled = locateDisabledReason !== null;

    // ── "Add to Chat" disabled reason ─────────────────────────────────
    // Mirrors locateDisabledReason so the UX hint is identical. A file
    // that belongs to a different agent or workspace must NOT be
    // attached to the current session's chat context — that would smuggle
    // cross-agent paths into another agent's prompt AND would 401/403 at
    // the gateway when the LLM tries to read it (workspace-scoped auth).
    //
    // Note: "loading" is intentionally NOT a reason here. A user can still
    // attach a file mid-load; the attached-context payload only needs the
    // relPath/absPath, not the loaded content.
    type AddToChatDisabledReason = "no-file" | "url-preview" | "wrong-agent" | "wrong-workspace" | "read-only-session" | null;
    const addToChatDisabledReason: AddToChatDisabledReason = (() => {
        if (!activeFile) return "no-file";
        if (activeFile.kind === "url") return "url-preview";
        if (activeFile.agentId !== selectedAgentId) return "wrong-agent";
        if (currentWorkspaceId !== null && activeFile.workspaceId !== currentWorkspaceId) return "wrong-workspace";
        // ADR-076: an attachment only ships with a message we are not
        // allowed to send, so attaching to a read-only session is a no-op.
        if (readOnlySession) return "read-only-session";
        return null;
    })();

    const handleLocateInTree = useCallback(() => {
        if (!activeFile || locateDisabled || !selectedAgentId || !activeSessionId) return;
        // Force the workspace panel to be visible — the AppLayout effect
        // consumes workspacePanelRequestSeq and expands the panel.
        requestShowWorkspacePanel();
        requestLocate({
            agentId: activeFile.agentId,
            workspaceId: activeFile.workspaceId,
            sessionId: activeSessionId,
            // Virtual tabs (diff/log) prefix relPath for monaco URI
            // disambiguation — FileTree locate must use the real path.
            relPath: sourceRelPath(activeFile),
        });
    }, [activeFile, locateDisabled, selectedAgentId, activeSessionId, requestShowWorkspacePanel, requestLocate]);

    // Resolve workspace root for LSP URI mapping.
    // Monaco's Uri.parse() cannot handle Windows file URIs (file:///C:/...),
    // so we use relPath as the model path (producing file:///core/... which
    // Monaco accepts). The LSP layer then maps relative URIs to absolute ones
    // using the workspace root. See lspProviders.ts → toLspUri().
    const workspaceRoot = useMemo(() => {
        if (!activeFile) return undefined;
        const node = useFileTreeStore.getState().getNode(treeKey(activeFile.agentId, activeFile.workspaceId, ""));
        return isReadyNode(node) ? node.root : undefined;
    }, [activeFile]);

    // Determine the active language for LSP — preview-mode and virtual
    // diff/log tabs don't need LSP.
    const lspLanguage =
        activeFile && activeFile.mode === "edit" && activeFile.kind === "file"
            ? activeFile.language
            : null;

    // Compute the set of all languages open in EDIT tabs (for pool lifecycle).
    // Preview-mode tabs are excluded — they are read-only and don't need LSP.
    const openLanguages = useMemo(() => {
        const langs = new Set<string>();
        for (const file of openFiles) {
            if (file.mode === "edit" && file.kind === "file" && file.language && !file.loading)
                langs.add(file.language);
        }
        return langs;
    }, [openFiles]);

    // LSP pool is enabled when there is at least one open language
    const lspEnabled = openLanguages.size > 0;

    // LSP client pool — maintains connections for all open languages,
    // disconnects only after a 30s grace period once a language's last file closes.
    const { activeStatus: lspStatus, activeStatusMessage: lspStatusMessage, activeClient: lspClient } = useLspClientPool(
        lspLanguage,
        openLanguages,
        activeFile?.agentId,
        activeFile?.workspaceId,
        lspEnabled,
        workspaceRoot
    );

    // Diagnostic logging — only when key inputs change, not on every render.
    useEffect(() => {
        log.debug(
            "[LSP] FileEditorPanel — lspLanguage:", lspLanguage,
            "status:", lspStatus,
            "lspEnabled:", lspEnabled,
        );
    }, [lspLanguage, lspStatus, lspEnabled]);

    // ── editorStatusStore mirroring (PR-2 of the unified status-bar refactor) ──
    // After PR-3 the cursor + selectedCount mirrors became redundant —
    // the Monaco selection handler writes straight to the store. The
    // LSP signals and reset effects below stay because they are driven
    // by `useLspClientPool` (which is NOT a singleton — AppLayout would
    // otherwise open a duplicate LSP WebSocket per language) and by
    // lifecycle transitions respectively.
    // See `stores/editorStatusStore.ts` for the data model.

    // Mirror the four LSP signals together (one action keeps partial
    // updates coherent — see the bundle comment in `editorStatusStore`).
    useEffect(() => {
        useEditorStatusStore.getState().setLspSignals({
            enabled: lspEnabled,
            language: lspLanguage,
            status: lspStatus,
            statusMessage: lspStatusMessage,
        });
    }, [lspEnabled, lspLanguage, lspStatus, lspStatusMessage]);

    // Reset to idle when the active file goes away or is loading — prevents
    // stale "Ln 42, Col 13" leaking into the global status bar. The dep
    // array uses both `id` (file switch) and `loading` (file became ready),
    // so this fires at every meaningful transition.
    useEffect(() => {
        if (!activeFile || activeFile.loading) {
            useEditorStatusStore.getState().resetToIdle();
        }
    }, [activeFile?.id, activeFile?.loading]);

    // Reset on unmount (panel closes via `AppLayout.hasOpenFiles === false`).
    useEffect(() => {
        return () => {
            useEditorStatusStore.getState().resetToIdle();
        };
    }, []);

    // Jump to cursorLine when search result navigates to a file.
    // Must handle two cases:
    //   1. File already active → same model, navigate directly.
    //   2. New file (loading) → model not yet switched; defer via
    //      pendingNavigationRef so onDidChangeModel applies it.
    useEffect(() => {
        const line = activeFile?.cursorLine;
        if (!line) return;

        const ed = editorRef.current;
        let sameModel = false;
        if (ed) {
            const model = ed.getModel();
            if (model) {
                const modelPath = model.uri.path.replace(/^\/+/, "");
                sameModel = modelPath === activeFile!.relPath;
            }
        }

        if (sameModel) {
            // Editor already has the right model — navigate immediately.
            ed!.revealLineInCenter(line);
            ed!.setPosition({ lineNumber: line, column: 1 });
        } else {
            // Model hasn't switched yet (or editor not mounted) —
            // defer to onDidChangeModel for correct model.
            pendingNavigationRef.current = { line, column: 1 };
        }

        // Clear cursorLine so re-renders don't re-jump
        useFileEditorStore.setState((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === activeFile!.id ? { ...f, cursorLine: undefined } : f,
            ),
        }));
    }, [activeFile?.id, activeFile?.cursorLine]);

    // Determine Monaco theme based on app theme.
    // For "system" mode we read `osTheme` from the settings store, which
    // stays in sync with macOS appearance via a matchMedia listener
    // registered once in the store (see settingsStore.ts). This avoids
    // each component registering its own listener.
    const osTheme = useSettingsStore((s) => s.osTheme);
    const monacoTheme = useMemo(() => {
        if (theme === "dark") return "vs-dark";
        if (theme === "light") return "vs";
        return osTheme === "dark" ? "vs-dark" : "vs";
    }, [theme, osTheme]);

    const resolvedMonacoTheme = monacoTheme;

    // Compute Monaco editor font size in pixels from global fontSize (rem-based).
    // Root font size is 16px by browser default, so fontSize rem * 16 = px.
    const editorFontSize = useMemo(() => Math.round(fontSize * 16), [fontSize]);

    const handleEditorMount: OnMount = useCallback((editor, monaco) => {
        editorRef.current = editor;
        monacoRef.current = monaco;
        // Track cursor position + selection + "Add to Chat" button position
        editor.onDidChangeCursorSelection((e) => {
            // Write cursor + selection straight to the cross-component
            // status store so the global-bar <FileStatusCluster> sees
            // every selection change without an intermediate local
            // useState. PR-3 removed the per-file status bar that used
            // to be the only consumer of these locals.
            const { setCursor, setSelectedCount } = useEditorStatusStore.getState();
            setCursor({
                line: e.selection.positionLineNumber,
                column: e.selection.positionColumn,
            });
            // Sync selection count and "Add to Chat" button
            const sel = e.selection;
            if (sel && !sel.isEmpty()) {
                const model = editor.getModel();
                if (model) {
                    setSelectedCount(model.getValueInRange(sel).length);
                    setSelectionRange({
                        startLine: sel.startLineNumber,
                        endLine: sel.endLineNumber,
                    });
                    // Position the floating button below selection end, clamped to container
                    requestAnimationFrame(() => {
                        const ed = editorRef.current;
                        if (!ed) return;
                        const endPos = ed.getScrolledVisiblePosition({
                            lineNumber: sel.endLineNumber,
                            column: sel.endColumn,
                        });
                        if (endPos) {
                            // Button is ~120px wide; clamp left so it stays inside the editor
                            const btnWidth = 120;
                            const containerWidth = ed.getLayoutInfo().width;
                            const left = Math.max(8, Math.min(endPos.left + 20, containerWidth - btnWidth));
                            setAddToChatPos({ top: endPos.top + 18, left });
                        }
                    });
                    return;
                }
            }
            setSelectedCount(0);
            setSelectionRange(null);
            setAddToChatPos(null);
        });

        // Handle model switches — the authoritative lifecycle hook for both
        // tab switches (driven by `path` prop change) and LSP peek-widget
        // cross-file navigation (driven by ICodeEditorService.openCodeEditor).
        // The Editor instance is no longer recreated per file, so this fires
        // synchronously when @monaco-editor/react calls editor.setModel().
        editor.onDidChangeModel(() => {
            const newModel = editor.getModel();
            if (!newModel) {
                log.debug("[LSP] onDidChangeModel — model is null");
                return;
            }

            // Only process file:// URIs — ignore inmemory://, output://, etc.
            const scheme = newModel.uri.scheme;
            if (scheme !== 'file') {
                log.debug("[LSP] onDidChangeModel — ignoring non-file model:", newModel.uri.toString());
                return;
            }

            // The model's URI path is the relative path (e.g. "core/runtime/src/foo.rs")
            const relPath = newModel.uri.path.replace(/^\/+/, "");
            log.debug("[LSP] onDidChangeModel — new relPath:", relPath, "uri:", newModel.uri.toString());

            // Restore previously saved view state for this model unless a
            // pending navigation will override it below.
            if (!pendingNavigationRef.current) {
                const savedState = viewStatesRef.current.get(relPath);
                if (savedState) {
                    // eslint-disable-next-line @typescript-eslint/no-explicit-any
                    editor.restoreViewState(savedState as any);
                }
            }

            // Apply pending cross-file navigation (takes priority over restored view state).
            // Deferred via requestAnimationFrame so that @monaco-editor/react's internal
            // viewState restoration (which runs AFTER onDidChangeModel) does not override
            // our navigation target.
            if (pendingNavigationRef.current) {
                const nav = pendingNavigationRef.current;
                pendingNavigationRef.current = null;
                requestAnimationFrame(() => {
                    const ed = editorRef.current;
                    if (!ed) return;
                    const currentModel = ed.getModel();
                    const lineCount = currentModel ? currentModel.getLineCount() : nav.line;
                    const safeLine = Math.min(Math.max(nav.line, 1), lineCount);
                    ed.setPosition({ lineNumber: safeLine, column: nav.column });
                    ed.revealLineInCenter(safeLine);
                    if (nav.endColumn !== undefined) {
                        ed.setSelection({
                            startLineNumber: safeLine,
                            startColumn: nav.column,
                            endLineNumber: nav.endLineNumber ?? safeLine,
                            endColumn: nav.endColumn,
                        });
                        ed.revealRangeInCenter({
                            startLineNumber: safeLine,
                            startColumn: nav.column,
                            endLineNumber: nav.endLineNumber ?? safeLine,
                            endColumn: nav.endColumn,
                        });
                    }
                    log.debug(`[LSP] onDidChangeModel — deferred navigation applied to line: ${safeLine}`);
                });
            }

            // Sync store active file when the model switch was triggered by
            // Monaco internals (peek widget) rather than React state. If the
            // store's active file already matches relPath, this is a no-op.
            const store = useFileEditorStore.getState();
            const activeFile = store.openFiles.find((f) => f.id === store.activeFileId);
            if (activeFile && activeFile.relPath === relPath) {
                log.debug("[LSP] onDidChangeModel — same file as active, skipping store sync");
                return;
            }

            const existingFile = store.openFiles.find((f) => f.relPath === relPath);
            if (existingFile) {
                log.debug("[LSP] onDidChangeModel — activating existing tab:", existingFile.id);
                store.setActiveFile(existingFile.id);
                return;
            }

            // The file isn't open — it must be a model created by ensureModelsForUris
            // for LSP cross-file reference preview. Open it via the store, which
            // re-uses the existing model content (already fetched).
            if (activeFile) {
                log.debug("[LSP] onDidChangeModel — cross-file navigation, opening:", relPath);
                void store.openFile(activeFile.agentId, activeFile.workspaceId, relPath);
            }
        });

        // Ctrl+S / Cmd+S to save
        editor.addCommand(
            // eslint-disable-next-line no-bitwise
            2048 | 49, // KeyMod.CtrlCmd | KeyCode.KeyS
            () => {
                const currentId = useFileEditorStore.getState().activeFileId;
                if (currentId) void saveFile(currentId);
            },
        );

        // GFM table cell navigation (Tab / Shift+Tab) — shared with the doc
        // editor (editorAid + tableAid). The Editor is a singleton reused
        // across all files (models switch via the `path` prop), so the
        // languageId filter keeps default Tab behavior for non-markdown
        // models. No dispose needed: commands die with the editor instance.
        registerMarkdownTableNavigation(editor, monaco, { languageId: "markdown" });

        // Ctrl+P / Cmd+P — Go to File (Monaco QuickInput-style palette).
        // Monaco standalone has no built-in "Go to File" provider and
        // IQuickInputService is not accessible from the editor's local DI
        // container, so we render a custom React component that replicates
        // the QuickInput visual style (same colors, typography, layout).
        // KeyCode.KeyP = 46 in monaco-editor 0.55.x (NOT 80).
        editor.addCommand(
            // eslint-disable-next-line no-bitwise
            2048 | 46, // KeyMod.CtrlCmd | KeyCode.KeyP
            () => {
                setShowGoToFile(true);
            },
        );

        // Ctrl+Shift+P / Cmd+Shift+P — Command Palette (Go to File).
        // In VS Code this opens the Command Palette; here we reuse the
        // GoToFilePalette since a dedicated command palette doesn't exist yet.
        // Without this, the event bubbles to the browser and triggers Print.
        // KeyCode.KeyP = 46 in monaco-editor 0.55.x.
        editor.addAction({
            id: "acowork.commandPalette",
            label: "Command Palette",
            keybindings: [
                // eslint-disable-next-line no-bitwise
                monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyP,
            ],
            run: () => {
                setShowGoToFile(true);
            },
        });

        // Ctrl+Shift+F / Cmd+Shift+F — Global search (ADR-081). The old
        // editor-internal GlobalSearchPanel was merged into the app-wide
        // GlobalSearchDialog (文件 tab uses the same ripgrep backend). Monaco
        // intercepts the combo while the editor is focused; AppLayout's
        // window-level keydown covers everywhere else — both open the same
        // dialog via searchStore.
        // KeyCode.KeyF = 33 in monaco-editor 0.55.x.
        // Use addAction (not addCommand) to ensure the keybinding overrides
        // any built-in Monaco action that may silently consume the event.
        editor.addAction({
            id: "acowork.globalSearch",
            label: "Global Search",
            keybindings: [
                // eslint-disable-next-line no-bitwise
                monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyF,
            ],
            run: () => {
                useSearchStore.getState().openDialog();
            },
        });

        // Ctrl+T / Cmd+T — Go to Symbol in Workspace (LSP workspace/symbol).
        // Mirrors VS Code's Ctrl+T exactly: opens a `#`-prefixed input box
        // backed by LSP semantic search (functions, classes, variables, …).
        // KeyCode.KeyT = 44 in monaco-editor 0.55.x.
        // Note: also intercept Ctrl+Shift+T via the same action to avoid
        // browser re-opening of recently-closed tabs.
        editor.addAction({
            id: "acowork.symbolSearch",
            label: "Go to Symbol in Workspace",
            keybindings: [
                // eslint-disable-next-line no-bitwise
                monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyT,
            ],
            run: () => {
                log.debug("[SymbolSearch] addAction fired — opening panel");
                setShowSymbolSearch(true);
            },
        });

        // ── Override ICodeEditorService.openCodeEditor ───────────────
        // In Monaco standalone, the default ICodeEditorService.openCodeEditor()
        // can only navigate within the same file. For cross-file navigation
        // (from LSP peek widgets like definition/references), it returns null.
        // We override it to detect cross-file navigation and switch the
        // active file in the store, which causes the editor to remount via
        // key={activeFile.id} with the target file loaded.
        if (!codeEditorOverriddenRef.current) {
            // Diagnostic: inspect what internal services are available
            const editorAny = editor as any;
            const svcKeys = Object.keys(editorAny).filter(k => k.toLowerCase().includes("service") || k.toLowerCase().includes("codeeditor"));
            // Use console.warn so it stands out in the console
            log.warn("[LSP] ═══ Editor internal service keys:", svcKeys);
            log.warn("[LSP] ═══ _codeEditorService:", !!editorAny._codeEditorService,
                "openCodeEditor:", !!editorAny._codeEditorService?.openCodeEditor);
            log.warn("[LSP] ═══ _instantiationService:", !!editorAny._instantiationService);

            let codeEditorSvc = editorAny._codeEditorService;

            // Fallback: try to get ICodeEditorService via _instantiationService
            if (!codeEditorSvc && editorAny._instantiationService) {
                try {
                    const instSvc = editorAny._instantiationService;
                    // Try common service access patterns
                    if (typeof instSvc.invokeFunction === "function") {
                        codeEditorSvc = instSvc.invokeFunction((accessor: any) => {
                            // Try known service IDs
                            for (const id of ["codeEditorService", "ICodeEditorService", "codeEditor"]) {
                                try { return accessor.get(id); } catch { /* skip */ }
                            }
                            return null;
                        });
                        log.debug("[LSP] _instantiationService lookup result:", !!codeEditorSvc);
                    }
                } catch (e) {
                    log.warn("[LSP] _instantiationService lookup failed:", e);
                }
            }

            if (codeEditorSvc?.openCodeEditor) {
                const originalOpenCodeEditor = codeEditorSvc.openCodeEditor.bind(codeEditorSvc);
                codeEditorSvc.openCodeEditor = async (
                    // eslint-disable-next-line @typescript-eslint/no-explicit-any
                    input: any,
                    // eslint-disable-next-line @typescript-eslint/no-explicit-any
                    source: any
                    // eslint-disable-next-line @typescript-eslint/no-explicit-any
                ): Promise<any> => {
                    log.debug("[LSP] openCodeEditor — input.resource:", input?.resource?.toString(),
                        "selection:", JSON.stringify(input?.options?.selection));

                    // Try default behavior first (same-file navigation)
                    const result = await originalOpenCodeEditor(input, source);
                    if (result) {
                        // Same-file navigation — Monaco's default handler returned
                        // an editor, but it may not correctly apply position/selection
                        // for subsequent navigations within the same file. We must
                        // explicitly apply the selection to ensure the cursor moves.
                        const selection = input?.options?.selection;
                        if (selection && editorRef.current) {
                            const pos = { lineNumber: selection.startLineNumber, column: selection.startColumn };
                            editorRef.current.setPosition(pos);
                            editorRef.current.revealLineInCenter(pos.lineNumber);
                            // If selection has an end range, set the full selection
                            if (selection.endLineNumber && selection.endColumn) {
                                editorRef.current.setSelection({
                                    startLineNumber: selection.startLineNumber,
                                    startColumn: selection.startColumn,
                                    endLineNumber: selection.endLineNumber,
                                    endColumn: selection.endColumn,
                                });
                            }
                            log.debug(`[LSP] openCodeEditor — same-file nav, applied selection to line: ${pos.lineNumber}`);
                        } else {
                            log.debug("[LSP] openCodeEditor — default handled it (same file, no selection to apply)");
                        }
                        return result;
                    }

                    // Cross-file navigation: the default service couldn't handle it
                    const targetUri = input?.resource;
                    const selection = input?.options?.selection;
                    if (!targetUri) {
                        log.warn("[LSP] openCodeEditor — no target URI, giving up");
                        return null;
                    }

                    // Not a file URI — let Monaco handle natively (inmemory://, output://, etc.)
                    if (targetUri.scheme !== 'file') {
                        log.debug("[LSP] openCodeEditor — ignoring non-file URI:", targetUri.toString());
                        return null;
                    }

                    // Extract relPath from model URI (e.g. file:///core/.../foo.rs → core/.../foo.rs)
                    const relPath = targetUri.path.replace(/^\/+/, "");
                    log.debug("[LSP] openCodeEditor — cross-file navigation to:", relPath);

                    // Check if the target file is already the active file — in this
                    // case setActiveFile() won't change the model, so onDidChangeModel
                    // won't fire and pendingNavigationRef won't be consumed. We must
                    // apply the position directly.
                    const store = useFileEditorStore.getState();
                    const currentActiveFile = store.openFiles.find((f) => f.id === store.activeFileId);
                    if (currentActiveFile && currentActiveFile.relPath === relPath) {
                        // Target is already the active file — defer position application
                        // to avoid being overridden by any internal Monaco state restore.
                        if (selection) {
                            const sel = selection;
                            requestAnimationFrame(() => {
                                const ed = editorRef.current;
                                if (!ed) return;
                                const pos = { lineNumber: sel.startLineNumber, column: sel.startColumn };
                                ed.setPosition(pos);
                                ed.revealLineInCenter(pos.lineNumber);
                                if (sel.endLineNumber && sel.endColumn) {
                                    ed.setSelection({
                                        startLineNumber: sel.startLineNumber,
                                        startColumn: sel.startColumn,
                                        endLineNumber: sel.endLineNumber,
                                        endColumn: sel.endColumn,
                                    });
                                    ed.revealRangeInCenter({
                                        startLineNumber: sel.startLineNumber,
                                        startColumn: sel.startColumn,
                                        endLineNumber: sel.endLineNumber,
                                        endColumn: sel.endColumn,
                                    });
                                }
                                log.debug(`[LSP] openCodeEditor — deferred navigation applied (same file) to line: ${pos.lineNumber}`);
                            });
                        }
                        return editorRef.current ?? null;
                    }

                    // Store target position for applying after model switch
                    if (selection) {
                        pendingNavigationRef.current = {
                            line: selection.startLineNumber,
                            column: selection.startColumn,
                            endLineNumber: selection.endLineNumber,
                            endColumn: selection.endColumn,
                        };
                    }

                    // Switch to the target file
                    const existingFile = store.openFiles.find((f) => f.relPath === relPath);

                    if (existingFile) {
                        log.debug("[LSP] openCodeEditor — activating existing tab:", existingFile.id);
                        store.setActiveFile(existingFile.id);
                    } else {
                        if (currentActiveFile) {
                            // Check if a Monaco model already exists for this file
                            // (created by ensureModelsForUris). If so, reuse its
                            // content to avoid a second fetch and ensure the line
                            // numbers match the reference locations.
                            const monacoInst = monacoRef.current;
                            const targetMonacoUri = monacoInst?.Uri.parse(relPath);
                            const existingModel = targetMonacoUri
                                ? monacoInst!.editor.getModel(targetMonacoUri)
                                : null;

                            if (existingModel && monacoInst) {
                                const content = existingModel.getValue();
                                const lang = existingModel.getLanguageId();
                                log.debug("[LSP] openCodeEditor — reusing model content, lines:", content.split("\n").length);
                                store.openFileWithContent(
                                    currentActiveFile.agentId, currentActiveFile.workspaceId,
                                    relPath, content, lang
                                );
                            } else {
                                log.debug("[LSP] openCodeEditor — opening new file (fetch):", relPath);
                                void store.openFile(currentActiveFile.agentId, currentActiveFile.workspaceId, relPath);
                            }
                        }
                    }

                    return null; // We handled navigation via React state
                };
                codeEditorOverriddenRef.current = true;
                log.warn("[LSP] ═══ ICodeEditorService.openCodeEditor OVERRIDDEN — cross-file navigation enabled");
            } else {
                log.warn("[LSP] ═══ Could not access _codeEditorService — cross-file navigation won't work");
            }
        }

        // Note: handleEditorMount only runs once now (Editor is no longer keyed
        // by activeFile.id), so all listeners and the openCodeEditor override
        // above are registered exactly once for the lifetime of this panel.
    }, [saveFile]);

    // ── Save view state before model switch ──────────────────────────────
    // The cleanup of this effect fires during React's effect-cleanup phase,
    // BEFORE @monaco-editor/react's setup effect calls editor.setModel() with
    // the new path. At that moment the editor still has the previous model
    // bound, so saveViewState() captures the outgoing file's cursor/scroll/
    // selection state. We restore it inside onDidChangeModel when the model
    // switches back.
    const activeReadyRelPath = activeFile && !activeFile.loading ? activeFile.relPath : undefined;
    useEffect(() => {
        return () => {
            if (editorRef.current && activeReadyRelPath) {
                const state = editorRef.current.saveViewState();
                if (state) {
                    viewStatesRef.current.set(activeReadyRelPath, state);
                }
            }
        };
    }, [activeReadyRelPath]);

    // ── Document Tracker lifecycle (bound to workspaceRoot) ────────────
    useEffect(() => {
        if (workspaceRoot) {
            documentTrackerRef.current = new LspDocumentTracker(workspaceRoot);
        }
        return () => {
            documentTrackerRef.current?.dispose(lspClient ?? null);
            documentTrackerRef.current = null;
        };
    }, [workspaceRoot]);

    // ── Track open documents via LspDocumentTracker ──────────────────────
    // When the editor mounts a new file (tab switch or cross-file navigation),
    // notify the LSP server via the tracker. Also handles LSP client
    // reconnection by re-opening all previously tracked documents.
    useEffect(() => {
        if (!lspClient || !workspaceRoot || !activeFile || activeFile.loading) return;
        if (!monacoRef.current) return;
        const tracker = documentTrackerRef.current;
        if (!tracker) return;

        // Detect LSP client reconnection — re-open all tracked documents
        if (prevLspClientRef.current !== null && prevLspClientRef.current !== lspClient) {
            log.debug("[LSP] DocumentTracker: client reconnected, re-opening all tracked docs");
            tracker.reopenAll(lspClient, monacoRef.current);
        }
        prevLspClientRef.current = lspClient;

        // Track the current active model as open
        const relPath = activeFile.relPath;
        const monacoUri = monacoRef.current.Uri.parse(relPath);
        const model = monacoRef.current.editor.getModel(monacoUri);
        if (model) {
            tracker.trackOpen(lspClient, model);
        }
    }, [activeFile, lspClient, workspaceRoot]);

    // ── Unpin opened tabs from the preview-model LRU pool ───────────────
    // Any file currently shown in a tab must not be LRU-evicted by
    // ensureModelsForUris peek-widget activity. Unpin them here so the
    // pool only tracks transient preview models.
    useEffect(() => {
        const monacoInst = monacoRef.current;
        if (!monacoInst) return;
        for (const f of openFiles) {
            const uriStr = monacoInst.Uri.parse(f.relPath).toString();
            unpinPreviewModel(uriStr);
        }
    }, [openFiles]);

    // ── Register file disposer with fileEditorStore ──────────────────────
    //
    // The store calls this disposer once per removed file BEFORE mutating
    // openFiles[]. It handles Monaco TextModel release (which the store
    // itself cannot do -- it has no Monaco reference) plus LSP didClose.
    //
    // Pre-registration, every caller of closeFile / closeOthers /
    // closeAllFiles had to remember to do its own cleanup, and at least
    // one caller (workspaceFsEvents reacting to a disk-delete) forgot --
    // leaking TextModels in monaco.editor.getModels().
    useEffect(() => {
        const unregister = registerFileDisposer((file) => {
            const monacoInst = monacoRef.current;
            // 1. LSP didClose -- only if the document is actually tracked
            //    AND the LSP client is still alive for the right workspace.
            if (lspClient && monacoInst && documentTrackerRef.current && file.kind === "file") {
                const uri = monacoInst.Uri.parse(file.relPath);
                const model = monacoInst.editor.getModel(uri);
                if (model) {
                    documentTrackerRef.current.trackClose(lspClient, model);
                }
            }
            // 2. Monaco TextModel disposal -- no-op if already disposed
            //    by another path.
            if (monacoInst && file.kind === "file") {
                disposeModelForFile(monacoInst, file.relPath);
            }
            // 3. Per-file view-state cleanup -- cursor/scroll position
            //    for a closed file is meaningless after this point.
            viewStatesRef.current.delete(file.relPath);
        });
        return unregister;
    }, [lspClient]);

    // ── LSP providers registration ──────────────────────────────────────

    useEffect(() => {
        // Unregister previous providers
        if (lspProvidersRef.current) {
            lspProvidersRef.current.dispose();
            lspProvidersRef.current = null;
        }

        // Register providers when both monaco and LSP client are ready
        if (monacoRef.current && lspClient && lspLanguage && workspaceRoot && activeFile) {
            try {
                log.debug("[LSP] Registering providers for:", lspLanguage, "client:", !!lspClient);
                lspProvidersRef.current = registerLspProviders(monacoRef.current, {
                    client: lspClient,
                    language: lspLanguage,
                    workspaceRoot,
                    agentId: activeFile.agentId,
                    workspaceId: activeFile.workspaceId,
                });
            } catch (err) {
                log.warn("[LSP] Failed to register providers:", err);
            }
        } else {
            log.debug("[LSP] Skipping provider registration — monaco:", !!monacoRef.current, "client:", !!lspClient, "language:", lspLanguage);
        }

        return () => {
            if (lspProvidersRef.current) {
                lspProvidersRef.current.dispose();
                lspProvidersRef.current = null;
            }
        };
    }, [lspClient, lspLanguage]);

    /** Add selected lines to chat context */
    const handleAddSelectionToChat = useCallback(() => {
        const agentId = selectedAgentId;
        if (!agentId || !activeFile || !selectionRange) return;
        // Defensive: refuse to attach a file owned by a different agent or
        // workspace. The button is hidden by `addToChatDisabledReason` in
        // the UI, but a programmatic invocation (or a stale closure) could
        // still land here. Returning silently is correct — the UI already
        // surfaced the reason via `locateDisabled.<reason>` tooltip text.
        if (activeFile.agentId !== agentId) {
            log.warn(
                "[FileEditor] Add to Chat refused: file belongs to a different agent",
                { fileAgent: activeFile.agentId, selectedAgent: agentId },
            );
            return;
        }
        const sessionId = getActiveSessionId(agentId);
        if (!sessionId) return;

        const { startLine, endLine } = selectionRange;
        const lineLabel = startLine === endLine ? `L${startLine}` : `L${startLine}-L${endLine}`;
        // Resolve absolute path from tree root
        const workspaceRoot = getCachedWorkspaceRoot(agentId, activeFile.workspaceId);
        const absPath = workspaceRoot ? `${workspaceRoot}/${activeFile.relPath}` : activeFile.relPath;
        addAttachedContext(agentId, sessionId, {
            id: `${agentId}:${activeFile.relPath}:${startLine}:${endLine}`,
            type: "selection",
            name: `${activeFile.relPath.split("/").pop() || activeFile.relPath} ${lineLabel}`,
            absPath,
            startLine,
            endLine,
        });
    }, [selectedAgentId, activeFile, selectionRange, getActiveSessionId, addAttachedContext]);

    const handleEditorChange = useCallback((value: string | undefined) => {
        if (value === undefined) return;
        const currentId = useFileEditorStore.getState().activeFileId;
        if (currentId) updateContent(currentId, value);
    }, [updateContent]);

    const handleClose = useCallback((e: React.MouseEvent, file: OpenFile) => {
        e.stopPropagation();
        if (file.dirty) {
            setClosingFileId(file.id);
            return;
        }
        // The registered file disposer handles LSP didClose + Monaco
        // TextModel disposal automatically when closeFile runs.
        closeFile(file.id);
    }, [closeFile]);

    const confirmClose = useCallback(() => {
        if (!closingFileId) return;
        // The registered file disposer handles LSP didClose + Monaco TextModel
        // disposal automatically when closeFile runs (with force=true).
        closeFile(closingFileId, true);
        setClosingFileId(null);
    }, [closingFileId, closeFile]);

    // ── Tab right-click menu (VSCode-style: Close / Close Others / Close All) ──

    const handleTabContextMenu = useCallback((e: React.MouseEvent, file: OpenFile) => {
        tabMenu.openAt(e, { fileId: file.id });
    }, [tabMenu]);

    // ── Tab right-click menu actions ──────────────────────────────────

    /**
     * Attach a workspace file (the one represented by the right-clicked tab) to
     * the active chat session's attached-context list. Mirrors the workspace
     * tree's "Add to Chat" handler so both surfaces share the same payload shape.
     *
     * URL-preview tabs (kind === "url") are skipped — they have no workspace
     * relPath/absPath to attach.
     */
    const handleTabAddToChat = useCallback((file: OpenFile) => {
        if (file.kind !== "file") return;
        const agentId = selectedAgentId;
        if (!agentId) return;
        // Defensive: refuse to attach a file owned by a different agent.
        // Right-click menu disables the item via `canAddToChat`, but we
        // double-check here in case of a stale closure or a direct call
        // path that bypasses the menu.
        if (file.agentId !== agentId) {
            log.warn(
                "[FileEditor] Tab Add to Chat refused: file belongs to a different agent",
                { fileAgent: file.agentId, selectedAgent: agentId },
            );
            return;
        }
        const sessionId = activeSessionId;
        if (!sessionId) return;
        // Resolve absolute path the same way as the floating selection addToChat.
        const root = getCachedWorkspaceRoot(agentId, file.workspaceId);
        const absPath = root ? `${root}/${file.relPath}` : file.relPath;
        addAttachedContext(agentId, sessionId, {
            id: `${agentId}:${file.relPath}`,
            type: "file",
            name: file.fileName,
            absPath,
        });
    }, [selectedAgentId, activeSessionId, addAttachedContext]);

    /**
     * Re-fetch the right-clicked tab's content from Gateway, replacing both
     * the displayed content and the originalContent baseline. This silently
     * discards any local edits — for dirty files we ask for confirmation
     * first (matching the FileTree delete-confirm fallback pattern) to avoid
     * surprising the user.
     */
    const handleTabRefresh = useCallback(async (file: OpenFile) => {
        if (file.kind !== "file" || file.loading) return;
        if (file.dirty) {
            const confirmed = window.confirm(
                `This file has unsaved changes. Discard and reload from disk?`,
            );
            if (!confirmed) return;
        }
        await refreshFile(file.id);
    }, [refreshFile]);

    const handleCloseTab = useCallback((file: OpenFile) => {
        if (file.dirty) {
            // Reuse the single-file unsaved-changes dialog
            setClosingFileId(file.id);
            return;
        }
        // The registered file disposer handles disposal automatically.
        closeFile(file.id);
    }, [closeFile]);

    const handleCloseOthers = useCallback((file: OpenFile) => {
        const others = openFiles.filter((f) => f.id !== file.id);
        if (others.length === 0) return;
        // Activate the kept tab first so the surviving tab is the focused one
        // (the store will also do this, but doing it here keeps the visual
        // transition smoother and avoids intermediate focus shifts).
        setActiveFile(file.id);
        const dirtyCount = others.filter((f) => f.dirty).length;
        if (dirtyCount > 0) {
            setBatchCloseRequest({
                kind: "others",
                fileIds: others.map((f) => f.id),
                dirtyCount,
                keepFileId: file.id,
            });
            return;
        }
        // closeOthers triggers the registered file disposer for each
        // non-kept tab -- no manual cleanup needed.
        closeOthers(file.id);
    }, [openFiles, closeOthers, setActiveFile]);

    const handleCloseAll = useCallback(() => {
        if (openFiles.length === 0) return;
        const dirtyCount = openFiles.filter((f) => f.dirty).length;
        if (dirtyCount > 0) {
            setBatchCloseRequest({
                kind: "all",
                fileIds: openFiles.map((f) => f.id),
                dirtyCount,
            });
            return;
        }
        // closeAllFiles triggers the registered file disposer for every
        // tab -- no manual cleanup needed.
        closeAllFiles();
    }, [openFiles, closeAllFiles]);

    const handleTabPreview = useCallback((file: OpenFile) => {
        if (file.kind !== "file") return;
        // openPreview is idempotent: if the file is already open it activates it
        // and switches the mode in place, otherwise it opens it as a preview tab.
        openPreview(file.agentId, file.workspaceId, file.relPath);
    }, [openPreview]);

    // Switch the active tab back from image-preview mode to edit (Monaco) mode.
    // Mirrors MarkdownPreviewView's `handleOpenAsEditor` so SVG users get the
    // same double-click affordance as Markdown readers — edit the source by
    // double-clicking the rendered preview. No-op for non-file kinds (URL tabs).
    const handleImagePreviewDoubleClick = useCallback(() => {
        const file = activeFile;
        if (!file || file.kind !== "file") return;
        // openFile() is idempotent: when the tab is already open it activates
        // it and flips `mode: "preview" → "edit"` in place. See fileEditorStore.openFile.
        void openFile(file.agentId, file.workspaceId, file.relPath);
    }, [activeFile, openFile]);

    // Toggle the active tab between preview and source (Monaco) mode.
    // Both store actions are idempotent and preserve the in-memory content:
    // openPreview switches an already-open tab to "preview" without refetching,
    // openFile switches it back to "edit" without refetching.
    const handleTogglePreview = useCallback(() => {
        const file = activeFile;
        if (!file || !supportsPreviewToggle(file)) return;
        if (file.mode === "preview") {
            void openFile(file.agentId, file.workspaceId, file.relPath);
        } else {
            void openPreview(file.agentId, file.workspaceId, file.relPath);
        }
    }, [activeFile, openFile, openPreview]);

    // Tab right-click menu items. Built only when the right-clicked file
    // changes or any of the relevant gates (active agent/session, file
    // kind, loading state, mode) flip — so the disabled flags stay
    // accurate without rebuilding on every render.
    const tabMenuItems = useMemo<ContextMenuItem<{ fileId: string }>[]>(() => {
        const target = tabMenu.payload?.fileId
            ? openFiles.find((f) => f.id === tabMenu.payload!.fileId)
            : undefined;
        if (!target) return [];
        const canCloseOthers = openFiles.length > 1;
        const canCloseAll = openFiles.length > 0;
        // Add to Chat / Refresh only apply to workspace files — URL-preview
        // tabs have no relPath/absPath to attach and no Gateway endpoint
        // to refetch.
        const showFileActions = target.kind === "file";
        const canAddToChat = showFileActions && addToChatDisabledReason === null && !!activeSessionId;
        const canRefresh = showFileActions && !target.loading;
        // Preview is only useful for files that have a preview view
        // (Markdown / HTML / SVG), and only when the tab is currently in source
        // mode — switching preview → preview would be a no-op. SVG is included
        // because its text source is editable in Monaco while the rendered form
        // is shown in the preview branch (image mimeType `image/svg+xml`).
        // Raster images (png/jpg/gif/webp) are intentionally excluded: their
        // "source" is just the base64 blob, so a preview pane adds no value.
        const canPreview = showFileActions
            && target.mode === "edit"
            && supportsPreviewToggle(target);

        const items: ContextMenuItem<{ fileId: string }>[] = [];

        if (showFileActions) {
            items.push({
                key: "add-to-chat",
                icon: <MessageSquarePlus size={14} />,
                label: t("workspace.contextMenu.addToChat"),
                disabled: !canAddToChat,
                onClick: () => { if (canAddToChat) handleTabAddToChat(target); },
            });
            items.push({
                key: "refresh",
                icon: <RefreshCw size={14} />,
                label: t("fileEditor.refresh"),
                disabled: !canRefresh,
                onClick: () => { if (canRefresh) void handleTabRefresh(target); },
            });
            items.push({
                key: "open-preview",
                icon: <Eye size={14} />,
                label: t("fileEditor.openPreview"),
                disabled: !canPreview,
                title: canPreview ? undefined : t("fileEditor.previewDisabled"),
                onClick: () => { if (canPreview) handleTabPreview(target); },
            });
            // Close-group items below are visually separated from the
            // "view" group (Add to Chat / Refresh / Preview) by a divider.
            items.push({
                key: "close",
                icon: <X size={14} />,
                label: t("fileEditor.close"),
                dividerBefore: true,
                onClick: () => handleCloseTab(target),
            });
        } else {
            // URL-preview tabs have no "view" group — divider before close
            // would be visually orphaned, so just put the close item.
            items.push({
                key: "close",
                icon: <X size={14} />,
                label: t("fileEditor.close"),
                onClick: () => handleCloseTab(target),
            });
        }
        items.push({
            key: "close-others",
            icon: <XSquare size={14} />,
            label: t("fileEditor.closeOthers"),
            disabled: !canCloseOthers,
            onClick: () => { if (canCloseOthers) handleCloseOthers(target); },
        });
        items.push({
            key: "close-all",
            icon: <Files size={14} />,
            label: t("fileEditor.closeAll"),
            disabled: !canCloseAll,
            onClick: () => { if (canCloseAll) handleCloseAll(); },
        });
        return items;
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [
        tabMenu.payload?.fileId,
        openFiles,
        selectedAgentId,
        activeSessionId,
        t,
    ]);

    const confirmBatchClose = useCallback(() => {
        if (!batchCloseRequest) return;
        // Both closeAllFiles(true) and closeOthers(..., true) trigger the
        // registered file disposer for every removed file -- no pre-clean
        // pass needed.
        if (batchCloseRequest.kind === "all") {
            closeAllFiles(true);
        } else if (batchCloseRequest.kind === "others" && batchCloseRequest.keepFileId) {
            closeOthers(batchCloseRequest.keepFileId, true);
        }
        setBatchCloseRequest(null);
    }, [batchCloseRequest, closeAllFiles, closeOthers]);

    return (
        <div
            ref={rootRef}
            data-panel="file-editor"
            className={cn(CAPSULE_PANE_CN, "relative shrink-0 bg-right-panel")}
            style={{ width }}
        >
            {/* Tab bar — bg must match the Monaco editor surface (vs=#FFFFFF in
                light, vs-dark=#1E1E1E in dark) so the strip and the editor body
                read as a single panel. Previously used `bg-right-panel` which is
                #F3F3F3 in light, producing a visible two-tone header above the
                white editor body. The dark values of `right-panel` and `vs-dark`
                already coincide (#1E1E1E), so the `dark:bg-right-panel` arm is a
                no-op there but kept for explicitness.
                `pt-[5px]` (not `mt-[5px]`) so the 5 px of top padding is filled
                by the tab strip's own bg rather than leaking the outer panel's
                `bg-right-panel` (#F3F3F3) and reintroducing the same seam. */}
            <div className="flex bg-white dark:bg-right-panel select-none px-0.5 gap-0.5 pt-[5px] border-b border-border-divider">
                <ScrollableTabBar
                    activeItemSelector={activeFileId ? `[data-file-id="${activeFileId}"]` : undefined}
                    activeItemId={activeFileId ?? undefined}
                >
                    {openFiles.map((file) => {
                        const isActive = file.id === activeFileId;
                        const isPreview = file.mode === "preview";
                        return (
                            <Tooltip
                                content={isPreview ? `${sourceRelPath(file)} · ${t("fileEditor.previewBadge")}` : sourceRelPath(file)}
                                variant="plain"
                                key={file.id}
                            >
                                <TabItem
                                    data-file-id={file.id}
                                    onClick={() => setActiveFile(file.id)}
                                    onContextMenu={(e) => handleTabContextMenu(e, file)}
                                    active={isActive}
                                >
                                    {/* File type icon — mirrors the file tree's icon (Seti UI).
                                        URL-preview tabs (kind === "url") have no meaningful
                                        extension mapping (fileName is a hostname like "example.com"),
                                        so they fall back to a plain generic icon. */}
                                    {file.kind === "file" ? (
                                        <SetiIcon {...getFileIcon(file.fileName)} size={14} />
                                    ) : (
                                        // Generic file glyph for URL tabs — keeps alignment with
                                        // file-type tabs while signalling "not a workspace file".
                                        <FileText className="h-3 w-3 shrink-0 text-text-tertiary" />
                                    )}
                                    {/* Loading / dirty indicator. Preview-mode tabs no longer
                                        render a separate badge here — the right-side toggle button
                                        already exposes the current mode, and adding another icon
                                        next to the file-type icon would crowd the tab. */}
                                    {file.loading ? (
                                        <Loader2 className="h-3 w-3 shrink-0 animate-spin text-text-tertiary" />
                                    ) : file.dirty ? (
                                        <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-[var(--color-accent)]" />
                                    ) : null}
                                    {/* File name */}
                                    <span className="min-w-0 flex-1 truncate text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)]">
                                        {file.fileName}
                                    </span>
                                    {/* Close button */}
                                    <Tooltip content={t("fileEditor.close")} variant="plain">
                                        <button
                                            onClick={(e) => handleClose(e, file)}
                                            className={cn(
                                                "shrink-0 rounded p-0.5 transition-opacity",
                                                isActive
                                                    ? "opacity-60 hover:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-600"
                                                    : "opacity-0 group-hover:opacity-60 hover:!opacity-100 hover:bg-zinc-300 dark:hover:bg-zinc-600",
                                            )}
                                        >
                                            <X className="h-3 w-3" />
                                        </button>
                                    </Tooltip>
                                </TabItem>
                            </Tooltip>
                        );
                    })}
                </ScrollableTabBar>

                {/* Right-side action group: Locate-in-tree + Preview/source toggle + Save.
                    Wrapped in a single flex container so the two buttons can sit
                    flush next to each other with their own gap, while the whole
                    group keeps a right-edge margin (pr-2) away from the panel edge.
                    Each button is locked to a fixed 20×20 square (h-5 w-5 + inline-flex)
                    — without that, the parent flex container's intrinsic height
                    (~28px from py-[--tab-py] + line-height) would stretch the buttons
                    vertically, producing a tall-and-narrow hit area. */}
                <div className="flex items-center gap-1 pr-2 shrink-0">
                    {/* Locate-in-tree button — only for workspace files (not URL previews).
                        Disabled (greyed out) when the file belongs to a different agent
                        or a different workspace than the currently active session. */}
                    {activeFile && (
                        <Tooltip
                            content={locateDisabledReason
                                ? t(`fileEditor.locateDisabled.${locateDisabledReason}`)
                                : t("fileEditor.locateInTree")}
                            variant="plain"
                        >
                            <button
                                aria-label={t("fileEditor.locateInTree")}
                                onClick={handleLocateInTree}
                                disabled={locateDisabled}
                                className={cn(
                                "inline-flex items-center justify-center rounded h-6 w-6 transition-colors",
                                locateDisabled
                                    ? "text-text-secondary  cursor-not-allowed"
                                    : "text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300",
                            )}
                            >
                                <Locate className="h-3.5 w-3.5" />
                            </button>
                        </Tooltip>
                    )}

                    {/* Preview/source toggle — only for files that support both modes
                        (Markdown / HTML / SVG). Shown in both directions so users can
                        flip without opening the tab context menu. */}
                    {activeFile && !activeFile.loading && supportsPreviewToggle(activeFile) && (
                        <Tooltip
                            content={
                                activeFile.mode === "preview"
                                    ? t("fileEditor.showSource")
                                    : t("fileEditor.openPreview")
                            }
                            variant="plain"
                        >
                            <button
                                aria-label={
                                    activeFile.mode === "preview"
                                        ? t("fileEditor.showSource")
                                        : t("fileEditor.openPreview")
                                }
                                onClick={handleTogglePreview}
                                className="inline-flex items-center justify-center rounded h-6 w-6 transition-colors text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
                            >
                                {activeFile.mode === "preview" ? (
                                    <Code2 className="h-3.5 w-3.5" />
                                ) : (
                                    <Eye className="h-3.5 w-3.5" />
                                )}
                            </button>
                        </Tooltip>
                    )}

                    {/* Save button — always visible for workspace files so the
                        action group stays a fixed width when toggling preview ↔
                        source. The button is disabled (greyed out) in three states:
                          1. not dirty            → no changes to write
                          2. saving in flight     → spinner takes over
                          3. preview mode         → read-only view, save N/A
                        Each disabled reason gets its own tooltip copy so the
                        greying never feels mysterious. */}
                    {activeFile && !activeFile.loading && activeFile.kind === "file" && (
                        <Tooltip
                                content={
                                    activeFile.saveError
                                        ? activeFile.saveError
                                        : activeFile.mode === "preview"
                                          ? t("fileEditor.saveDisabledPreview")
                                          : t("fileEditor.save")
                                }
                                variant="plain"
                            >
                            <button
                                onClick={() => activeFile.dirty && void saveFile(activeFile.id)}
                                disabled={!activeFile.dirty || activeFile.saving || activeFile.mode === "preview"}
                                aria-label={
                                    activeFile.saveError
                                        ? `Save failed: ${activeFile.saveError}`
                                        : activeFile.mode === "preview"
                                          ? t("fileEditor.saveDisabledPreview")
                                          : t("fileEditor.save")
                                }
                                className={cn(
                                    "inline-flex items-center justify-center rounded h-6 w-6 transition-colors",
                                    activeFile.mode === "preview"
                                        ? "text-text-secondary  cursor-not-allowed"
                                        : activeFile.saveError
                                          ? "text-red-500 hover:bg-red-100 dark:hover:bg-red-900/30"
                                          : activeFile.dirty
                                            ? "text-[var(--color-accent)] hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
                                            : "text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 cursor-default",
                                )}
                            >
                                {activeFile.saving ? (
                                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                                ) : activeFile.saveError ? (
                                    <AlertCircle className="h-3.5 w-3.5" />
                                ) : (
                                    <Save className="h-3.5 w-3.5" />
                                )}
                            </button>
                        </Tooltip>
                    )}
                </div>
            </div>

            {/* Editor area — Editor is mounted whenever there is at least one
                open file. Switching tabs changes `path` (and therefore the
                Monaco model) without recreating the Editor instance, so LSP
                cross-file navigation no longer races with editor remounts.

                No inner border: the outer wrapper's `border border-right-
                panel-border` is the panel's silhouette, and the tab bar's
                own `border-b` is the divider between the tab strip and the
                Monaco surface below. */}
            <div className="relative flex flex-1 flex-col overflow-hidden">
                {!activeFile ? (
                    <div className="flex h-full items-center justify-center text-xs text-text-tertiary ">
                        {t("fileEditor.emptyState")}
                    </div>
                ) : activeFile.kind === "url" ? (
                    <UrlPreviewView url={activeFile.url || activeFile.relPath} fileName={activeFile.fileName} />
                ) : activeFile.kind === "diff" ? (
                    // ADR-078 decision 7 — read-only side-by-side diff of the
                    // original (HEAD) vs. working-tree content. GitDiffKind
                    // "deleted" arrives with empty `modified`, which Monaco
                    // renders as an empty right pane.
                    <>
                        {activeFile.gitDiffKind === "binary" ? (
                            // ADR-078 decision 4/7 — binary diffs degrade to a
                            // placeholder: /git/diff returns kind=binary with no
                            // content, so show an explicit notice rather than two
                            // empty panes.
                            <div className="flex h-full items-center justify-center text-xs text-text-tertiary ">
                                {t("gitStatus.binaryDiff")}
                            </div>
                        ) : (
                            <div className="flex h-full flex-col">
                                {/* Labels the two side-by-side panes. Backend
                                    pins original=HEAD and modified=working-
                                    tree when cached=0, which is what
                                    GitStatusPanel.openDiff always sends
                                    (fetchDiff(..., 0) — see
                                    core/acowork-runtime/src/usecases/
                                    git_query_impl.rs `diff`). grid-cols-2
                                    keeps each label visually under its pane. */}
                                <div className="grid grid-cols-2 border-b border-right-panel-border bg-zinc-50 text-11 dark:bg-zinc-900/40">
                                    <button
                                        type="button"
                                        onClick={(e) => {
                                            const next = pickerSide === "base" ? null : "base";
                                            setBaseAnchor(next ? (e.currentTarget as HTMLElement) : null);
                                            setPickerSide(next);
                                        }}
                                        aria-label={t("gitStatus.headLabel")}
                                        className={
                                            "flex items-center gap-2 truncate px-3 py-1 text-left transition-colors " +
                                            (pickerSide === "base"
                                                ? "bg-blue-100 text-blue-800 dark:bg-blue-900/50 dark:text-blue-200"
                                                : "text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-800/60")
                                        }
                                    >
                                        <span className="font-medium">
                                            {(!activeFile.diffBaseRef || activeFile.diffBaseRef === "HEAD")
                                                ? t("gitStatus.headLabel")
                                                : activeFile.diffBaseRef.slice(0, 7)}
                                        </span>
                                        <span className="truncate text-text-tertiary ">
                                            {sourceRelPath(activeFile)}
                                        </span>
                                        <svg
                                            className="ml-auto h-3 w-3 shrink-0 opacity-60"
                                            viewBox="0 0 12 12"
                                            fill="none"
                                            stroke="currentColor"
                                            strokeWidth="1.5"
                                            aria-hidden
                                        >
                                            <path d="M3 4.5L6 7.5L9 4.5" />
                                        </svg>
                                    </button>
                                    <button
                                        type="button"
                                        onClick={(e) => {
                                            const next = pickerSide === "head" ? null : "head";
                                            setHeadAnchor(next ? (e.currentTarget as HTMLElement) : null);
                                            setPickerSide(next);
                                        }}
                                        aria-label={t("gitStatus.workingTreeLabel")}
                                        className={
                                            "flex items-center justify-end gap-2 truncate px-3 py-1 text-right transition-colors " +
                                            (pickerSide === "head"
                                                ? "bg-blue-100 text-blue-800 dark:bg-blue-900/50 dark:text-blue-200"
                                                : "text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-800/60")
                                        }
                                    >
                                        <span className="truncate text-text-tertiary ">
                                            {sourceRelPath(activeFile)}
                                        </span>
                                        <span className="font-medium">
                                            {!activeFile.diffHeadRef
                                                ? t("gitStatus.workingTreeLabel")
                                                : activeFile.diffHeadRef.slice(0, 7)}
                                        </span>
                                        <svg
                                            className="h-3 w-3 shrink-0 opacity-60"
                                            viewBox="0 0 12 12"
                                            fill="none"
                                            stroke="currentColor"
                                            strokeWidth="1.5"
                                            aria-hidden
                                        >
                                            <path d="M3 4.5L6 7.5L9 4.5" />
                                        </svg>
                                    </button>
                                </div>
                                {pickerSide && (
                                    <CommitPicker
                                        anchorEl={pickerSide === "base" ? baseAnchor : headAnchor}
                                        currentRef={
                                            pickerSide === "base"
                                                ? (activeFile.diffBaseRef ?? "HEAD")
                                                : (activeFile.diffHeadRef ?? "")
                                        }
                                        allowWorkingTree={pickerSide === "head"}
                                        agentId={activeFile.agentId}
                                        workspaceId={activeFile.workspaceId}
                                        relPath={sourceRelPath(activeFile)}
                                        onSelect={(ref) => {
                                            if (pickerSide === "base") {
                                                void applyDiffRefs(ref, activeFile.diffHeadRef ?? "");
                                            } else {
                                                void applyDiffRefs(activeFile.diffBaseRef ?? "HEAD", ref);
                                            }
                                            setPickerSide(null);
                                            setBaseAnchor(null);
                                            setHeadAnchor(null);
                                        }}
                                        onClose={() => {
                                            setPickerSide(null);
                                            setBaseAnchor(null);
                                            setHeadAnchor(null);
                                        }}
                                    />
                                )}
                                <div className="min-h-0 flex-1">
                                    <DiffEditor
                                        original={activeFile.originalContent}
                                        modified={activeFile.content}
                                        language={activeFile.language}
                                        theme={resolvedMonacoTheme}
                                        onMount={(ed) => setDiffEditor(ed)}
                                        options={{
                                            minimap: { enabled: false },
                                            fontSize: editorFontSize,
                                            lineNumbers: "on",
                                            scrollBeyondLastLine: false,
                                            readOnly: true,
                                            // Diff mode follows the per-tab toggle
                                            // (OpenFile.diffInlineMode, seeded to
                                            // `true` by openVirtualFile for new diff
                                            // tabs). Inline is the default — single
                                            // pane with +/- colour blocks; flipping
                                            // to false restores the side-by-side
                                            // two-pane layout (ADR-078 decision 7).
                                            renderSideBySide: !activeFile.diffInlineMode,
                                            // ADR-078 decision 7: when side-by-side
                                            // is explicitly chosen, hold the two-
                                            // pane layout at any editor width.
                                            // Monaco otherwise auto-switches to
                                            // inline when the container is narrower
                                            // than renderSideBySideInlineBreakpoint
                                            // (default 900 px). For inline mode
                                            // the option is irrelevant — Monaco is
                                            // already single-pane.
                                            useInlineViewWhenSpaceIsLimited: false,
                                            // Disable Monaco's diff overview ruler. The
                                            // ruler paints a 30-px-wide marker strip on
                                            // the modified (right) pane only — see
                                            // monaco-editor/.../overviewRulerFeature.js
                                            // (ONE_OVERVIEW_WIDTH=15, ENTIRE=15*2=30) —
                                            // which stacks visually on top of the
                                            // 14-px scrollbar there. Result: the right
                                            // scrollbar looks ~2x wider than the left's
                                            // (which has no ruler). Diff markers are
                                            // still conveyed by the per-line green/red
                                            // highlights inside the panes, which is the
                                            // primary affordance.
                                            renderOverviewRuler: false,
                                            automaticLayout: true,
                                            padding: { top: 8 },
                                        }}
                                    />
                                </div>
                            </div>
                        )}
                        {/* Floating Up / Down hunk-jump + (log only)
                            Load-older overlay — sibling of the DiffEditor
                            so its `absolute right-4` anchors to the same
                            `relative` editor-area div. Wired to
                            Monaco's IStandaloneDiffEditor.goToDiff
                            (defined in monaco-editor editor.api.d.ts
                            IDiffEditor) — no custom hunk parsing needed. */}
                        <GitVirtualNav file={activeFile} diffEditor={diffEditor} />
                    </>
                ) : activeFile.kind === "log" ? (
                    // ADR-078 decision 7 — read-only single-pane commit log.
                    <>
                        <Editor
                            path={activeFile.relPath}
                            value={activeFile.content}
                            language={activeFile.language}
                            theme={resolvedMonacoTheme}
                            keepCurrentModel={false}
                            options={{
                                minimap: { enabled: false },
                                fontSize: editorFontSize,
                                lineNumbers: "off",
                                scrollBeyondLastLine: false,
                                readOnly: true,
                                wordWrap: "on",
                                automaticLayout: true,
                                padding: { top: 8 },
                            }}
                        />
                        {/* Floating Up / Down / Load-older overlay — see
                            diff branch comment above. Log tabs pass
                            `diffEditor={null}` because the load-older
                            button doesn't need the Monaco editor. */}
                        <GitVirtualNav file={activeFile} diffEditor={null} />
                    </>
                ) : activeFile.kind === "attachment" ? (
                    // Inbox chat attachment (ADR-076) — the body was fetched
                    // from the chat blob route by the caller and lives only in
                    // this tab. Strictly read-only: it is someone else's sent
                    // file, there is no workspace to write it back to, and
                    // `handleEditorChange` ignores edits for any non-"file"
                    // kind, so leaving the editor editable would silently
                    // swallow typing. Line numbers on and word wrap only for
                    // extensionless plain text (logs, csv) — same look as a
                    // workspace file otherwise.
                    <Editor
                        path={activeFile.relPath}
                        value={activeFile.content}
                        language={activeFile.language}
                        theme={resolvedMonacoTheme}
                        keepCurrentModel={false}
                        options={{
                            minimap: { enabled: false },
                            fontSize: editorFontSize,
                            lineNumbers: "on",
                            scrollBeyondLastLine: false,
                            readOnly: true,
                            wordWrap: activeFile.language === "plaintext" ? "on" : "off",
                            automaticLayout: true,
                            padding: { top: 8 },
                        }}
                    />
                ) : activeFile.mode === "preview" && activeFile.mimeType?.startsWith("image/") ? (
                    // SVG preview branch. Raster images (png/jpg/gif/webp) never
                    // reach here because `canPreview` in the tab context menu only
                    // enables "Open Preview" for .md/.html?/.svg — see the regex
                    // above. This means double-click-to-edit is a safe affordance:
                    // the source we switch back to is always editable XML/text.
                    //
                    // `bg-editor-canvas` paints the wrapper with the same Monaco `vs` / `vs-dark`
                    // background the source editor uses, so the right-hand preview column reads
                    // as one "editor canvas" distinct from the left ChatPanel bg (`#FAFAFA` /
                    // zinc-900). Previously this used zinc-50/zinc-900 (and then zinc-800),
                    // which collapsed into the chat surface and made the image look unframed.
                    // Token is registered in globals.css @theme + .dark block; keep it in sync
                    // with Monaco — do not hard-code #FFFFFF / #1E1E1E here.
                    <div
                        className="flex h-full w-full items-center justify-center bg-editor-canvas"
                        onDoubleClick={handleImagePreviewDoubleClick}
                        title={t("fileEditor.previewDoubleClickHint")}
                    >
                        {/* SVG is delivered as raw XML text (the gateway no longer
                            lists it in BINARY_EXTENSIONS, see
                            workspace_query_impl::BINARY_EXTENSIONS). Re-encode to
                            base64 here so the data URI is robust against `#`,
                            `<`, `>`, etc. that would otherwise need fragile
                            percent-encoding. Raster images come back already
                            base64-encoded by the gateway. */}
                        <img
                            src={
                                activeFile.mimeType === "image/svg+xml"
                                    ? `data:${activeFile.mimeType};base64,${encodeTextToBase64(activeFile.content)}`
                                    : `data:${activeFile.mimeType};base64,${activeFile.content}`
                            }
                            alt={activeFile.fileName}
                            className="max-h-full max-w-full object-contain"
                        />
                    </div>
                ) : activeFile.mode === "preview" && activeFile.mimeType === "text/html" ? (
                    <HtmlPreviewView
                        content={activeFile.content}
                        gatewayUrl={getGatewayUrl()}
                        agentId={activeFile.agentId}
                        workspaceId={activeFile.workspaceId}
                        relPath={activeFile.relPath}
                        fileName={activeFile.fileName}
                    />
                ) : activeFile.mode === "preview" ? (
                    <MarkdownPreviewView file={activeFile} />
                ) : monacoReady ? (
                    <>
                        {/* Markdown editing toolbar — only for .md source files;
                            shared with the doc editor (components/markdown). */}
                        {activeFile.language === "markdown" && (
                            <MarkdownToolbar editor={editorRef.current} disabled={activeFile.loading} />
                        )}
                        <div className="min-h-0 flex-1">
                        <Editor
                            path={activeReadyRelPath}
                            value={activeFile && !activeFile.loading ? activeFile.content : undefined}
                            language={activeFile && !activeFile.loading ? activeFile.language : undefined}
                            theme={resolvedMonacoTheme}
                            onChange={handleEditorChange}
                            onMount={handleEditorMount}
                            keepCurrentModel
                            options={{
                                minimap: { enabled: false },
                                fontSize: editorFontSize,
                                lineNumbers: "on",
                                scrollBeyondLastLine: false,
                                wordWrap: "off",
                                tabSize: 2,
                                renderWhitespace: "selection",
                                padding: { top: 8 },
                                automaticLayout: true,
                                readOnly: false,
                            }}
                        />
                        {/* Floating "Add to Chat" button near selection end */}
                        {selectionRange && addToChatPos && (
                            <button
                                onClick={handleAddSelectionToChat}
                                // Kept visible-but-disabled rather than hidden
                                // when the session is read-only: the button
                                // floats next to the user's own selection, so
                                // silently vanishing looks like a bug. The
                                // reason is still discoverable from the
                                // context menu's disabled item.
                                disabled={addToChatDisabledReason !== null}
                                title={addToChatDisabledReason === "read-only-session"
                                    ? t("chatPanel.readOnlySession")
                                    : undefined}
                                className="absolute z-30 flex items-center gap-1.5 rounded-md px-2 py-1 text-xs font-medium text-white shadow-md transition-colors disabled:cursor-not-allowed disabled:opacity-50"
                                style={{
                                    top: addToChatPos.top,
                                    left: addToChatPos.left,
                                    backgroundColor: "var(--color-accent)",
                                    borderColor: "color-mix(in srgb, var(--color-accent) 70%, black)",
                                    borderWidth: 1,
                                    borderStyle: "solid",
                                }}
                                onMouseEnter={(e) => { e.currentTarget.style.filter = "brightness(0.88)"; }}
                                onMouseLeave={(e) => { e.currentTarget.style.filter = ""; }}
                            >
                                <MessageSquarePlus className="h-3.5 w-3.5" />
                                Add to Chat
                            </button>
                        )}
                        {activeFile.loading && (
                            <div className="absolute inset-0 flex items-center justify-center gap-2 bg-right-panel/80 text-xs text-text-tertiary">
                                <Loader2 className="h-4 w-4 animate-spin" />
                                Loading...
                            </div>
                        )}
                        </div>
                    </>
                ) : monacoFailed ? (
                    <div className="flex h-full items-center justify-center gap-2 text-xs text-text-tertiary">
                        <AlertCircle className="h-4 w-4" />
                        Editor failed to load. Close and reopen the panel to retry.
                    </div>
                ) : (
                    <div className="flex h-full items-center justify-center gap-2 text-xs text-text-tertiary">
                        <Loader2 className="h-4 w-4 animate-spin" />
                        Loading editor...
                    </div>
                )}
            </div>

            {/* ADR-078 decision 6 — GitStatusBar moved to WorkspaceExplorer
                (workspace-panel bottom) per 2026-XX revision. The editor only
                still owns the *virtual diff/log* render branches (DiffEditor /
                readonly single-pane Monaco) which read `OpenFile.virtual` and
                are independent of gitStore. */}

            {/* Close confirmation dialog */}
            {closingFileId && (
                <div
                    className="fixed inset-0 z-[60] flex items-center justify-center bg-modal-overlay"
                    onClick={() => setClosingFileId(null)}
                >
                    <div
                        className="mx-4 w-full max-w-sm rounded-md border border-border-outer bg-modal-surface p-5 shadow-xl"
                        onClick={(e) => e.stopPropagation()}
                    >
                        <div className="flex items-start gap-3">
                            <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-amber-100 dark:bg-amber-900/30">
                                <FileText className="h-5 w-5 text-amber-600 dark:text-amber-400" />
                            </div>
                            <div className="flex-1">
                                <h3 className="text-sm font-medium text-text ">
                                    {t("fileEditor.unsavedChanges")}
                                </h3>
                                <p className="mt-1 text-xs text-text-tertiary ">
                                    {t("fileEditor.saveChanges")}
                                </p>
                            </div>
                        </div>
                        <div className="mt-4 flex justify-end gap-2">
                            <button
                                onClick={() => setClosingFileId(null)}
                                className="rounded btn-solid px-3 py-1.5 text-xs"
                            >
                                {t("fileEditor.cancel")}
                            </button>
                            <button
                                onClick={confirmClose}
                                className="rounded btn-accent px-3 py-1.5 text-xs"
                            >
                                {t("fileEditor.discard")}
                            </button>
                        </div>
                    </div>
                </div>
            )}

            {/* Go to File palette (Ctrl+P) */}
            {showGoToFile && activeFile && (
                <GoToFilePalette
                    agentId={activeFile.agentId}
                    workspaceId={activeFile.workspaceId}
                    onClose={() => {
                        setShowGoToFile(false);
                        editorRef.current?.focus();
                    }}
                />
            )}

            {/* Symbol Search panel (Ctrl+T) — Go to Symbol in Workspace (LSP) */}
            {showSymbolSearch && activeFile && (
                <SymbolSearchPanel
                    agentId={activeFile.agentId}
                    workspaceId={activeFile.workspaceId}
                    lspClient={lspClient}
                    workspaceRoot={workspaceRoot}
                    onClose={() => {
                        setShowSymbolSearch(false);
                        editorRef.current?.focus();
                    }}
                />
            )}

            {/* Tab right-click context menu (VSCode-style) — unified component. */}
            <ContextMenu<{ fileId: string }>
                isOpen={tabMenu.isOpen}
                menuProps={tabMenu.menuProps}
                items={tabMenuItems}
                payload={tabMenu.payload}
                selectionAtOpen={tabMenu.selectionAtOpen}
                onClose={tabMenu.close}
            />

            {/* Batch close confirmation (Close Others / Close All with dirty files) */}
            {batchCloseRequest && (
                <div
                    className="fixed inset-0 z-[60] flex items-center justify-center bg-modal-overlay"
                    onClick={() => setBatchCloseRequest(null)}
                >
                    <div
                        className="mx-4 w-full max-w-sm rounded-md border border-border-outer bg-modal-surface p-5 shadow-xl"
                        onClick={(e) => e.stopPropagation()}
                    >
                        <div className="flex items-start gap-3">
                            <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-amber-100 dark:bg-amber-900/30">
                                <FileText className="h-5 w-5 text-amber-600 dark:text-amber-400" />
                            </div>
                            <div className="flex-1">
                                <h3 className="text-sm font-medium text-text ">
                                    {t("fileEditor.unsavedChanges")}
                                </h3>
                                <p className="mt-1 text-xs text-text-tertiary ">
                                    {batchCloseRequest.kind === "all"
                                        ? t("fileEditor.batchCloseAllConfirm", {
                                            count: batchCloseRequest.dirtyCount,
                                        })
                                        : t("fileEditor.batchCloseOthersConfirm", {
                                            count: batchCloseRequest.dirtyCount,
                                        })}
                                </p>
                            </div>
                        </div>
                        <div className="mt-4 flex justify-end gap-2">
                            <button
                                onClick={() => setBatchCloseRequest(null)}
                                className="rounded btn-solid px-3 py-1.5 text-xs"
                            >
                                {t("fileEditor.cancel")}
                            </button>
                            <button
                                onClick={confirmBatchClose}
                                className="rounded btn-accent px-3 py-1.5 text-xs"
                            >
                                {t("fileEditor.discard")}
                            </button>
                        </div>
                    </div>
                </div>
            )}
        </div>
    );
}
