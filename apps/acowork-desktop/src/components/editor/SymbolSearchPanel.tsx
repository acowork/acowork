/**
 * SymbolSearchPanel — VS Code Ctrl+T style "Go to Symbol in Workspace" widget.
 *
 * Visual design mirrors GlobalSearchPanel / GoToFilePalette (Monaco QuickInput
 * clone): same 60% width, dark/light theme colors, same typography.
 *
 * Differences from a plain text search panel:
 *   - Search input is **prefixed with a static `#` prefix icon** (left side
 *     of the input box, not editable) to signal "this searches code symbols,
 *     not text".
 *   - There are **no Aa / ab / .\* toggle buttons** — symbol search is
 *     semantic (LSP `workspace/symbol`), so case/word/regex toggles don't
 *     apply.
 *   - Backed exclusively by the active LSP client via the workspace/symbol
 *     LSP request; no Gateway round-trip.
 *
 * If the LSP server does not implement `workspace/symbol` (e.g. some
 * language servers return `MethodNotFound` −32601), we show a graceful
 * "language server does not support symbol search" message instead of an
 * error.
 */

import { useState, useEffect, useRef, useMemo, useCallback } from "react";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { SetiIcon } from "../common/SetiIcon";
import { getFileIcon } from "../workspace/FileTree/fileIcons";
import type { MonacoLanguageClient } from "monaco-languageclient";
import { log } from "../../lib/logger";
import { readThemeVar } from "../../lib/themeVars";

/** A single LSP workspace/symbol result, normalized for display. */
interface LspSymbolResult {
    /** Symbol name (e.g. function name, class name). */
    name: string;
    /** Symbol kind as a short label: "fn", "class", "var", "iface", etc. */
    kindLabel: string;
    /** Relative file path within workspace. */
    file: string;
    /** 1-based line number. */
    line: number;
    /** 1-based column number. */
    column: number;
    /** Container name (e.g. parent class), if any. */
    containerName?: string;
}

interface SymbolSearchPanelProps {
    agentId: string;
    workspaceId: string;
    /** LSP client for the active language. If null, panel shows a "wait" state. */
    lspClient: MonacoLanguageClient | null;
    /** Absolute workspace root path (needed for URI → relPath conversion). */
    workspaceRoot?: string;
    onClose: () => void;
}

/* ─── VS Code Theme Colors (same as GlobalSearchPanel / GoToFilePalette) ── */

const darkTheme = {
    widgetBg: "#252526",
    inputBg: "#3C3C3C",
    inputFg: "#CCCCCC",
    inputBorder: "transparent",
    inputFocusBorder: "#007FD4",
    inputPlaceholder: "#969696",
    listFocusBg: "#04395E",
    listFocusFg: "#FFFFFF",
    listHoverBg: "#2A2D2E",
    highlight: "#2AAAFF",
    description: "rgba(204,204,204,0.7)",
    countBg: "#4D4D4D",
    countFg: "#FFFFFF",
    shadow: "rgba(0,0,0,0.36) 0 0 8px 2px",
    prefixFg: "#6A6A6A",
};

const lightTheme = {
    widgetBg: "#F3F3F3",
    inputBg: "#FFFFFF",
    inputFg: "#616161",
    inputBorder: "#CECECE",
    inputFocusBorder: "#0090F1",
    inputPlaceholder: "#767676",
    listFocusBg: "#0060C0",
    listFocusFg: "#FFFFFF",
    listHoverBg: "#E8E8E8",
    highlight: "#0066BF",
    description: "#616161",
    countBg: "#C4C4C4",
    countFg: "#616161",
    shadow: "rgba(0,0,0,0.16) 0 2px 8px",
    prefixFg: "#8A8A8A",
};

/* ─── LSP SymbolKind mapping (LSP spec) ──────────────────────────────── */

const LSP_SYMBOL_KIND_LABEL: Record<number, string> = {
    1: "file", 2: "mod", 3: "ns", 4: "pkg",
    5: "class", 6: "method", 7: "prop", 8: "field",
    9: "ctor", 10: "enum", 11: "iface", 12: "func",
    13: "var", 14: "const", 15: "string", 16: "num",
    17: "bool", 18: "array", 19: "obj", 20: "key",
    21: "null", 22: "enumMbr", 23: "struct", 24: "event",
    25: "op", 26: "tparam",
};

function symbolKindLabel(kind: number): string {
    return LSP_SYMBOL_KIND_LABEL[kind] ?? `kind(${kind})`;
}

/* ─── Scrollbar styling ─────────────────────────────────────────────── */

const scrollStyle = `
.symbol-search-list::-webkit-scrollbar { width: 6px; }
.symbol-search-list::-webkit-scrollbar-thumb { background: #4A4A4A; border-radius: 3px; }
.symbol-search-list::-webkit-scrollbar-thumb:hover { background: #5A5A5A; }
.symbol-search-list::-webkit-scrollbar-track { background: transparent; }
`;

/* ─── Main Component ────────────────────────────────────────────────── */

export function SymbolSearchPanel({
    agentId, workspaceId, lspClient, workspaceRoot, onClose,
}: SymbolSearchPanelProps) {
    const [query, setQuery] = useState("");
    const [focusedIdx, setFocusedIdx] = useState(0);
    const [loading, setLoading] = useState(false);
    const [results, setResults] = useState<LspSymbolResult[]>([]);
    const [searched, setSearched] = useState(false);
    const [error, setError] = useState<string | null>(null);
    /** True when LSP returns MethodNotFound (-32601) — server has no workspace/symbol. */
    const [unsupported, setUnsupported] = useState(false);
    const [inputFocused, setInputFocused] = useState(false);

    const abortRef = useRef<AbortController | null>(null);
    const inputRef = useRef<HTMLInputElement | null>(null);
    const listRef = useRef<HTMLDivElement | null>(null);
    const itemRefs = useRef<(HTMLDivElement | null)[]>([]);

    const theme = useSettingsStore((s) => s.theme);
    const isDark = useMemo(() => {
        if (theme === "dark") return true;
        if (theme === "light") return false;
        return document.documentElement.classList.contains("dark");
    }, [theme]);
    const colors = isDark ? darkTheme : lightTheme;

    /**
     * Open a file at a specific line and close the panel.
     * Mirrors the helper pattern used in GlobalSearchPanel / GoToFilePalette —
     * delegates the actual open to fileEditorStore, which is where open-tabs
     * and active-tab logic live.
     */
    const openFileAtLine = useCallback((file: string, line: number) => {
        void useFileEditorStore.getState().openFile(agentId, workspaceId, file, line);
        onClose();
    }, [agentId, workspaceId, onClose]);

    // 30-second timeout — jdtls can be slow for large Java projects
    const SYMBOL_SEARCH_TIMEOUT_MS = 30_000;

    /**
     * LSP workspace/symbol search.
     * Falls back gracefully when:
     *   - query is empty → clears results
     *   - LSP server is unavailable → does nothing (handled in render)
     *   - server returns MethodNotFound (-32601) → sets `unsupported` flag
     *   - server is slow → aborts after SYMBOL_SEARCH_TIMEOUT_MS
     */
    const doSymbolSearch = useCallback(async (q: string) => {
        if (abortRef.current) {
            abortRef.current.abort();
        }
        if (!q.trim() || !lspClient || !workspaceRoot) {
            setResults([]);
            setSearched(false);
            setError(null);
            setUnsupported(false);
            return;
        }

        const controller = new AbortController();
        abortRef.current = controller;
        setLoading(true);
        setError(null);
        setUnsupported(false);

        let timeoutId: ReturnType<typeof setTimeout> | undefined;

        try {
            const raw = (await Promise.race([
                lspClient.sendRequest("workspace/symbol", { query: q }),
                new Promise<never>((_, reject) => {
                    timeoutId = setTimeout(() => {
                        controller.abort();
                        reject(new DOMException("Symbol search timed out", "AbortError"));
                    }, SYMBOL_SEARCH_TIMEOUT_MS);
                }),
            ])) as any[];

            clearTimeout(timeoutId);
            if (controller.signal.aborted) return;

            if (!raw || raw.length === 0) {
                setResults([]);
                setSearched(true);
                setLoading(false);
                return;
            }

            // Normalize LSP SymbolInformation[] → LspSymbolResult[]
            const out: LspSymbolResult[] = [];
            const seen = new Set<string>();
            for (const si of raw) {
                let uri: string = si.location?.uri ?? "";
                let relPath = uri;
                if (uri.startsWith("file://")) {
                    let filePath = uri.slice("file://".length);
                    try { filePath = decodeURIComponent(filePath); } catch { /* keep as-is */ }
                    if (/^\/[A-Za-z]:/.test(filePath)) {
                        filePath = filePath.slice(1);
                    }
                    const root = workspaceRoot.replace(/\\/g, "/");
                    const fpNorm = filePath.replace(/\\/g, "/");
                    if (fpNorm.toLowerCase().startsWith(root.toLowerCase())) {
                        relPath = fpNorm.slice(root.length).replace(/^\//, "");
                    } else {
                        relPath = fpNorm;
                    }
                }
                // Deduplicate by (name + container + file)
                const dedupKey = `${si.name}\0${si.containerName ?? ""}\0${relPath}`;
                if (seen.has(dedupKey)) continue;
                seen.add(dedupKey);

                out.push({
                    name: si.name,
                    kindLabel: symbolKindLabel(si.kind),
                    file: relPath,
                    line: (si.location?.range?.start?.line ?? 0) + 1,
                    column: (si.location?.range?.start?.character ?? 0) + 1,
                    containerName: si.containerName,
                });
            }

            // Group by file, then by line within each file
            out.sort((a, b) => {
                const fc = a.file.localeCompare(b.file);
                return fc !== 0 ? fc : a.line - b.line;
            });

            setResults(out);
            setSearched(true);
        } catch (e: any) {
            clearTimeout(timeoutId);
            if (controller.signal.aborted) return;

            const code = e?.code;
            const msg: string = e?.message ?? "";
            const isAbort = e?.name === "AbortError" || msg.includes("timed out");
            const isMethodNotFound =
                code === -32601 ||
                msg.toLowerCase().includes("method not found");
            if (isAbort) {
                setError("Symbol search timed out");
            } else if (isMethodNotFound) {
                setUnsupported(true);
                setError(null);
            } else {
                log.error("[SymbolSearch] error:", e);
                setError(`Symbol search failed: ${msg || String(e)}`);
            }
            setResults([]);
            setSearched(true);
        }
        setLoading(false);
    }, [lspClient, workspaceRoot]);

    // Debounced search on query change
    useEffect(() => {
        const timer = setTimeout(() => doSymbolSearch(query), 200);
        return () => clearTimeout(timer);
    }, [query, doSymbolSearch]);

    // Auto-focus on mount
    useEffect(() => {
        requestAnimationFrame(() => inputRef.current?.focus());
    }, []);

    // ── Group by file ─────────────────────────────────────────────
    type NavItem =
        | { kind: "file"; file: string; symbolCount: number }
        | { kind: "symbol"; symbol: LspSymbolResult };

    const grouped = useMemo(() => {
        const map = new Map<string, LspSymbolResult[]>();
        for (const s of results) {
            const arr = map.get(s.file) ?? [];
            arr.push(s);
            map.set(s.file, arr);
        }
        return map;
    }, [results]);

    const flatItems = useMemo<NavItem[]>(() => {
        const items: NavItem[] = [];
        for (const [file, syms] of grouped) {
            items.push({ kind: "file", file, symbolCount: syms.length });
            for (const s of syms) {
                items.push({ kind: "symbol", symbol: s });
            }
        }
        return items;
    }, [grouped]);

    // Reset focused index when results change
    useEffect(() => {
        setFocusedIdx(0);
    }, [results]);

    // Scroll focused item into view
    useEffect(() => {
        const el = itemRefs.current[focusedIdx];
        if (el && listRef.current) {
            el.scrollIntoView({ block: "nearest", behavior: "smooth" });
        }
    }, [focusedIdx]);

    const closeAndFocus = useCallback(() => {
        onClose();
        // The editor focus restoration is handled by parent (FileEditorPanel)
    }, [onClose]);

    const handleKeyDown = useCallback((e: React.KeyboardEvent) => {
        if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            closeAndFocus();
            return;
        }

        if (flatItems.length === 0) return;

        if (e.key === "ArrowDown") {
            e.preventDefault();
            setFocusedIdx((i) => Math.min(i + 1, flatItems.length - 1));
        } else if (e.key === "ArrowUp") {
            e.preventDefault();
            setFocusedIdx((i) => Math.max(i - 1, 0));
        } else if (e.key === "Enter") {
            e.preventDefault();
            const item = flatItems[focusedIdx];
            if (!item) return;
            if (item.kind === "symbol") {
                openFileAtLine(item.symbol.file, item.symbol.line);
                closeAndFocus();
            } else {
                // File header — jump to first symbol in the group
                const next = flatItems[focusedIdx + 1];
                if (next?.kind === "symbol") {
                    openFileAtLine(next.symbol.file, next.symbol.line);
                    closeAndFocus();
                }
            }
        }
    }, [flatItems, focusedIdx, openFileAtLine, closeAndFocus]);

    const isReady = lspClient != null && workspaceRoot != null;

    return (
        <>
            <style>{scrollStyle}</style>
            {/* Backdrop — clips to the FileEditorPanel container (uses position: absolute,
                same pattern as GlobalSearchPanel / GoToFilePalette). */}
            <div
                onMouseDown={(e) => {
                    // Prevent the mousedown from reaching the editor surface below.
                    e.stopPropagation();
                }}
                onClick={closeAndFocus}
                style={{
                    position: "absolute", inset: 0,
                    backgroundColor: "rgba(0,0,0,0.3)",
                    zIndex: 2549,
                }}
            />

            {/* Panel — positioned absolutely relative to FileEditorPanel container.
                Width capped by `width` prop on the container, so we let the
                percentage be relative to that container rather than viewport. */}
            <div
                style={{
                    position: "absolute",
                    top: 40,
                    left: "50%",
                    transform: "translateX(-50%)",
                    width: "60%",
                    backgroundColor: readThemeVar("--color-widget-bg", colors.widgetBg),
                    boxShadow: colors.shadow,
                    borderRadius: 4,
                    zIndex: 2550,
                    fontFamily: "inherit",
                    fontSize: 13,
                    color: colors.inputFg,
                    overflow: "hidden",
                    userSelect: "none",
                }}
            >
                {/* Input row — static `#` prefix on the left */}
                <div
                    style={{
                        display: "flex", alignItems: "center",
                        padding: "0 8px", height: 30,
                        border: `1px solid ${inputFocused ? colors.inputFocusBorder : colors.inputBorder}`,
                        borderRadius: 2,
                        backgroundColor: colors.inputBg,
                    }}
                >
                    <span
                        title="Symbol search (LSP workspace/symbol)"
                        style={{
                            color: colors.prefixFg,
                            fontSize: 14, fontWeight: 600,
                            marginRight: 6, userSelect: "none",
                            fontFamily: "'Menlo','Monaco','Consolas',monospace",
                        }}
                    >
                        #
                    </span>
                    <input
                        ref={inputRef}
                        value={query}
                        onChange={(e) => setQuery(e.target.value)}
                        onKeyDown={handleKeyDown}
                        onFocus={() => setInputFocused(true)}
                        onBlur={() => setInputFocused(false)}
                        placeholder={
                            isReady
                                ? "Type a symbol name (e.g. parseConfig, MyClass)"
                                : "Waiting for language server..."
                        }
                        disabled={!isReady}
                        style={{
                            flex: 1,
                            backgroundColor: "transparent",
                            border: "none", outline: "none",
                            color: colors.inputFg,
                            fontSize: 13, lineHeight: "22px",
                            fontFamily: "inherit",
                        }}
                    />
                    {!isReady && (
                        <span style={{
                            fontSize: 11, color: colors.description,
                            marginLeft: 8, whiteSpace: "nowrap",
                        }}>
                            language server starting…
                        </span>
                    )}
                </div>

                {/* Results list */}
                <div
                    ref={listRef}
                    className="symbol-search-list"
                    style={{
                        maxHeight: 340,
                        overflowY: "auto",
                        padding: "4px 0",
                    }}
                >
                    {unsupported ? (
                        <div style={{ padding: "12px 16px", color: colors.description, fontSize: 12 }}>
                            The active language server does not support symbol search
                            (<code style={{ fontFamily: "monospace" }}>workspace/symbol</code>).
                            Try <kbd>Ctrl</kbd>+<kbd>P</kbd> to find files or
                            <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>F</kbd> to search content.
                        </div>
                    ) : error ? (
                        <div style={{
                            padding: "8px 12px",
                            color: isDark ? "#FF6B6B" : "#D32F2F",
                            fontSize: 12,
                        }}>
                            {error}
                        </div>
                    ) : !searched ? (
                        <div style={{ padding: "8px 12px", color: colors.description, fontSize: 12 }}>
                            {isReady
                                ? "Type to search workspace symbols (functions, classes, variables, …)"
                                : "Symbol search requires a running language server"}
                        </div>
                    ) : results.length === 0 ? (
                        <div style={{ padding: "8px 12px", color: colors.description, fontSize: 12 }}>
                            No symbols found
                        </div>
                    ) : (
                        flatItems.map((item, idx) => {
                            if (item.kind === "file") {
                                return (
                                    <div
                                        key={`file:${item.file}`}
                                        style={{
                                            display: "flex", alignItems: "center",
                                            padding: "4px 12px", marginTop: 4,
                                            color: colors.description,
                                            fontSize: 11, fontWeight: 600,
                                            textTransform: "uppercase", letterSpacing: 0.4,
                                            userSelect: "none",
                                            gap: 6,
                                        }}
                                    >
                                        <SetiIcon {...getFileIcon(item.file)} size={14} />
                                        <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                                            {item.file}
                                        </span>
                                        <span style={{
                                            marginLeft: "auto",
                                            backgroundColor: colors.countBg,
                                            color: colors.countFg,
                                            padding: "0 6px", borderRadius: 8,
                                            fontSize: 10, fontWeight: 600,
                                            letterSpacing: 0.2,
                                        }}>
                                            {item.symbolCount}
                                        </span>
                                    </div>
                                );
                            }
                            const s = item.symbol;
                            const focused = idx === focusedIdx;
                            return (
                                <div
                                    key={`${s.file}:${s.line}:${s.name}`}
                                    ref={(el) => { itemRefs.current[idx] = el; }}
                                    onMouseEnter={() => setFocusedIdx(idx)}
                                    onClick={() => {
                                        openFileAtLine(s.file, s.line);
                                        closeAndFocus();
                                    }}
                                    style={{
                                        display: "flex", alignItems: "center",
                                        padding: "0 12px", height: 22,
                                        cursor: "pointer",
                                        backgroundColor: focused ? colors.listFocusBg : "transparent",
                                        color: focused ? colors.listFocusFg : colors.inputFg,
                                        fontSize: 12,
                                    }}
                                >
                                    {/* Symbol kind badge */}
                                    <span style={{
                                        minWidth: 44, textAlign: "right",
                                        fontSize: 10, fontWeight: 600,
                                        color: colors.highlight,
                                        opacity: focused ? 1 : 0.8,
                                        marginRight: 8, flexShrink: 0,
                                        fontFamily: "inherit",
                                    }}>
                                        {s.kindLabel}
                                    </span>
                                    {/* Symbol name */}
                                    <span style={{
                                        fontWeight: 600,
                                        overflow: "hidden", textOverflow: "ellipsis",
                                        whiteSpace: "nowrap", flexShrink: 1,
                                    }}>
                                        {s.name}
                                    </span>
                                    {/* Container name (e.g. parent class) */}
                                    {s.containerName && (
                                        <span style={{
                                            marginLeft: 6, opacity: 0.65,
                                            overflow: "hidden", textOverflow: "ellipsis",
                                            whiteSpace: "nowrap", flexShrink: 1,
                                        }}>
                                            {s.containerName}
                                        </span>
                                    )}
                                    {/* Location hint */}
                                    <span style={{
                                        marginLeft: "auto", flexShrink: 0,
                                        opacity: focused ? 0.85 : 0.55,
                                        fontFamily: "'Menlo','Monaco','Consolas',monospace",
                                        fontSize: 11,
                                    }}>
                                        {s.file.split("/").pop()}:{s.line}
                                    </span>
                                </div>
                            );
                        })
                    )}
                </div>

                {/* Footer hint */}
                <div style={{
                    display: "flex", justifyContent: "space-between",
                    padding: "6px 10px",
                    borderTop: `1px solid ${isDark ? "#1E1E1E" : "#E5E5E5"}`,
                    color: colors.description, fontSize: 11,
                }}>
                    <span>
                        Go to Symbol in Workspace
                        {loading
                            ? " · searching…"
                            : results.length > 0 && ` · ${results.length} result${results.length === 1 ? "" : "s"}`}
                    </span>
                    <span>
                        <span style={{ marginRight: 12 }}>↑↓ navigate</span>
                        <span style={{ marginRight: 12 }}>↵ open</span>
                        <span>esc close</span>
                    </span>
                </div>
            </div>
        </>
    );
}
