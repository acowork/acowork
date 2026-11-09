/**
 * GoToFilePalette — A VS Code QuickInput-style file search widget.
 *
 * Visual design is copied from Monaco Editor's built-in QuickInput:
 *   - Responsive width (min(600px, calc(100% - 16px)))
 *   - Same dark/light theme colors as VS Code
 *   - Search input with right-aligned count badge
 *   - Scrollable list (max 20 items visible, 22px row height)
 *   - Fuzzy matching with bold highlight on matched characters
 *   - Same typography (13px, line-height 22px)
 *
 * Data source: uses workspaceStore.treeCache (same as WorkspaceExplorer)
 * so results are instant — no re-scan on every Ctrl+P.
 */

import { useState, useEffect, useRef, useMemo, useCallback } from "react";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { SetiIcon } from "../common/SetiIcon";
import { OverlayScrollTarget } from "../common/OverlayScrollbar";
import { OVERLAY_SCROLL_HOST_CLASS, OVERLAY_SCROLL_VIEWPORT_CLASS } from "../common/overlayScrollThumb";
import { getFileIcon } from "../workspace/FileTree/fileIcons";

/* ─── Types ────────────────────────────────────────────────────────────────── */

interface FileItem {
    label: string;
    description: string;
    relPath: string;
}

interface GoToFilePaletteProps {
    agentId: string;
    workspaceId: string;
    onClose: () => void;
}

/* ─── VS Code Theme Colors ──────────────────────────────────────────── */

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
};

/* ─── Fuzzy Match ───────────────────────────────────────────────────── */

function fuzzyMatch(text: string, query: string): { score: number; indices: number[] } | null {
    if (!query) return { score: 0, indices: [] };
    const lower = text.toLowerCase();
    const q = query.toLowerCase();
    const indices: number[] = [];
    let qi = 0;
    let score = 0;
    let prevIdx = -1;

    for (let i = 0; i < text.length && qi < q.length; i++) {
        if (lower[i] === q[qi]) {
            indices.push(i);
            if (i === 0 || " /.-_".includes(text[i - 1])) score += 8;
            else if (text[i] !== lower[i] && text[i - 1] === lower[i - 1]) score += 5;
            else if (prevIdx === i - 1) score += 2;
            else score += 1;
            prevIdx = i;
            qi++;
        }
    }
    return qi === q.length ? { score, indices } : null;
}

/* ─── Highlighted Label ─────────────────────────────────────────────── */

function HighlightedLabel({
    text,
    indices,
    highlightColor,
    baseColor,
}: {
    text: string;
    indices: number[];
    highlightColor: string;
    baseColor: string;
}) {
    if (!indices.length) {
        return <span style={{ color: baseColor, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{text}</span>;
    }
    const idxSet = new Set(indices);
    const parts: Array<{ char: string; highlighted: boolean }> = [];
    for (let i = 0; i < text.length; i++) {
        parts.push({ char: text[i], highlighted: idxSet.has(i) });
    }
    return (
        <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
            {parts.map((p, i) =>
                p.highlighted ? (
                    <span key={i} style={{ fontWeight: 700, color: highlightColor }}>{p.char}</span>
                ) : (
                    <span key={i} style={{ color: baseColor }}>{p.char}</span>
                ),
            )}
        </span>
    );
}

/* ─── Main Component ────────────────────────────────────────────────── */

export function GoToFilePalette({ agentId, workspaceId, onClose }: GoToFilePaletteProps) {
    const [query, setQuery] = useState("");
    const [focusedIdx, setFocusedIdx] = useState(0);
    const [loading, setLoading] = useState(false);
    const [inputFocused, setInputFocused] = useState(false);

    const inputRef = useRef<HTMLInputElement>(null);
    const listRef = useRef<HTMLDivElement>(null);
    const itemRefs = useRef<(HTMLDivElement | null)[]>([]);

    // We only call the server-side filename search. No treeCache
    // dependency: the lazy directory walk was removed because it
    // could not scale to deep trees within the keystroke deadline.
    const findFiles = useWorkspaceStore((s) => s.findFiles);

    const theme = useSettingsStore((s) => s.theme);
    const isDark = useMemo(() => {
        if (theme === "dark") return true;
        if (theme === "light") return false;
        return document.documentElement.classList.contains("dark");
    }, [theme]);
    const colors = isDark ? darkTheme : lightTheme;

    /* ── Server-side filename search (debounced + cancellable) ──── */
    const [rawResults, setRawResults] = useState<
        Array<{ name: string; relPath: string }>
    >([]);

    useEffect(() => {
        if (!query.trim() || !agentId) {
            setRawResults([]);
            setLoading(false);
            return;
        }
        const controller = new AbortController();
        setLoading(true);
        const timer = setTimeout(() => {
            void findFiles(agentId, workspaceId, query, 50, controller.signal).then((resp) => {
                if (controller.signal.aborted) return;
                if (!resp) {
                    setRawResults([]);
                    setLoading(false);
                    return;
                }
                setRawResults(
                    resp.matches.map((m) => ({ name: m.name, relPath: m.relPath })),
                );
                setLoading(false);
            });
        }, 150);
        return () => {
            clearTimeout(timer);
            controller.abort();
        };
    }, [query, agentId, workspaceId, findFiles]);

    /* ── Compute highlighted indices (matches against the live query) ── */
    const filtered = useMemo(() => {
        if (!query.trim()) return [];
        const rows: Array<
            FileItem & { score: number; nameIndices: number[]; pathIndices: number[] }
        > = [];
        for (const r of rawResults) {
            const slash = r.relPath.lastIndexOf("/");
            const dir = slash >= 0 ? r.relPath.slice(0, slash) : "";
            const label = r.name;
            const description = dir ? `${dir}/${label}` : label;
            const nameMatch = fuzzyMatch(label, query);
            const pathMatch = fuzzyMatch(description, query);
            const best =
                nameMatch && pathMatch
                    ? nameMatch.score >= pathMatch.score
                        ? nameMatch
                        : pathMatch
                    : nameMatch || pathMatch;
            if (!best) continue;
            rows.push({
                label,
                description,
                relPath: r.relPath,
                score: best.score + (nameMatch ? 10 : 0),
                nameIndices: nameMatch?.indices ?? [],
                pathIndices: pathMatch?.indices ?? [],
            });
        }
        rows.sort((a, b) => b.score - a.score);
        return rows.slice(0, 50);
    }, [rawResults, query]);

    /* ── Auto-focus input ──────────────────────────────────────────── */
    useEffect(() => {
        requestAnimationFrame(() => inputRef.current?.focus());
    }, []);

    /* ── Scroll focused item into view ─────────────────────────────── */
    useEffect(() => {
        const el = itemRefs.current[focusedIdx];
        el?.scrollIntoView({ block: "nearest" });
    }, [focusedIdx]);

    /* ── Reset focus when filtered list changes ────────────────────── */
    useEffect(() => {
        setFocusedIdx(0);
    }, [query]);

    /* ── Keyboard handler ──────────────────────────────────────────── */
    const handleKeyDown = useCallback(
        (e: React.KeyboardEvent) => {
            switch (e.key) {
                case "Escape":
                    e.preventDefault();
                    e.stopPropagation();
                    onClose();
                    break;
                case "ArrowDown":
                    e.preventDefault();
                    setFocusedIdx((i) => Math.min(i + 1, filtered.length - 1));
                    break;
                case "ArrowUp":
                    e.preventDefault();
                    setFocusedIdx((i) => Math.max(i - 1, 0));
                    break;
                case "Enter": {
                    e.preventDefault();
                    const item = filtered[focusedIdx];
                    if (item) {
                        void useFileEditorStore.getState().openFile(agentId, workspaceId, item.relPath);
                        onClose();
                    }
                    break;
                }
            }
        },
        [filtered, focusedIdx, agentId, workspaceId, onClose],
    );

    const openFile = useFileEditorStore.getState().openFile;

    return (
        <>
            {/* Backdrop — absolute within editor container */}
            <div
                style={{ position: "absolute", inset: 0, zIndex: 2549 }}
                onMouseDown={(e) => { e.stopPropagation(); onClose(); }}
            />
            {/* Widget — responsive width, centered in editor area */}
            <div
                style={{
                    position: "absolute",
                    top: 40,
                    left: "50%",
                    transform: "translateX(-50%)",
                    width: "60%",
                    zIndex: 2550,
                    backgroundColor: colors.widgetBg,
                    borderRadius: 6,
                    boxShadow: colors.shadow,
                    fontFamily: '-apple-system, BlinkMacSystemFont, "Inter", "Segoe UI", "Noto Sans SC", "Microsoft YaHei", Roboto, sans-serif',
                    fontSize: 13,
                    overflow: "hidden",
                    userSelect: "none",
                }}
                onKeyDown={handleKeyDown}
            >
                {/* ── Header (search input) ──────────────────────────── */}
                <div style={{ padding: "6px 6px 2px 6px" }}>
                    <div style={{ position: "relative", display: "flex", alignItems: "center" }}>
                        {/* Search icon */}
                        <svg width="16" height="16" viewBox="0 0 16 16"
                            style={{ position: "absolute", left: 8, flexShrink: 0, pointerEvents: "none" }}>
                            <circle cx="6.5" cy="6.5" r="4.5" stroke={colors.description} strokeWidth="1.2" fill="none" />
                            <line x1="10" y1="10" x2="14" y2="14" stroke={colors.description} strokeWidth="1.2" />
                        </svg>
                        <input
                            ref={inputRef}
                            type="text"
                            value={query}
                            onChange={(e) => setQuery(e.target.value)}
                            placeholder="Type file name to search..."
                            className="g2f-input"
                            style={{
                                flexGrow: 1,
                                height: 30,
                                padding: "0 40px 0 28px",
                                backgroundColor: colors.inputBg,
                                color: colors.inputFg,
                                border: `1px solid ${colors.inputBorder}`,
                                borderRadius: 2,
                                outline: inputFocused ? `2px solid ${colors.inputFocusBorder}` : "none",
                                outlineOffset: inputFocused ? "-2px" : "initial",
                                fontSize: 13,
                                lineHeight: "28px",
                            }}
                            onFocus={() => setInputFocused(true)}
                            onBlur={() => setInputFocused(false)}
                        />
                        {/* Count badge */}
                        {!loading && (
                            <span style={{
                                position: "absolute", right: 8, padding: "2px 4px", borderRadius: 2,
                                fontSize: 11, lineHeight: "normal",
                                backgroundColor: colors.countBg, color: colors.countFg,
                            }}>
                                {filtered.length}
                            </span>
                        )}
                    </div>
                </div>

                {/* ── List ───────────────────────────────────────────── */}
                {/* The palette is an absolute overlay over the editor, so the
                    scroller is hosted (positioned wrapper + hover reveal) and
                    the floating thumb is attached to the list itself. */}
                <div className={OVERLAY_SCROLL_HOST_CLASS}>
                <div
                    ref={listRef}
                    className={`g2f-list ${OVERLAY_SCROLL_VIEWPORT_CLASS}`}
                    style={{ maxHeight: 20 * 22, overflowY: "auto", lineHeight: "22px", paddingBottom: 5 }}
                >
                    {loading ? (
                        <div style={{ padding: "8px 12px", color: colors.description, fontSize: 12 }}>
                            Searching workspace...
                        </div>
                    ) : filtered.length === 0 ? (
                        <div style={{ padding: "8px 12px", color: colors.description, fontSize: 12 }}>
                            {query ? "No matching files" : "Type to search files"}
                        </div>
                    ) : (
                        filtered.map((item, idx) => {
                            const focused = idx === focusedIdx;
                            const nameIndices = (item as any).nameIndices ?? [];
                            const pathIndices = (item as any).pathIndices ?? [];
                            return (
                                <div
                                    key={item.relPath}
                                    ref={(el) => { itemRefs.current[idx] = el; }}
                                    onMouseEnter={() => setFocusedIdx(idx)}
                                    onClick={() => {
                                        void openFile(agentId, workspaceId, item.relPath);
                                        onClose();
                                    }}
                                    style={{
                                        display: "flex",
                                        alignItems: "center",
                                        padding: "0 6px",
                                        height: 22,
                                        cursor: "pointer",
                                        backgroundColor: focused ? colors.listFocusBg : "transparent",
                                        color: focused ? colors.listFocusFg : colors.inputFg,
                                        borderRadius: 3,
                                        margin: "0 6px",
                                    }}
                                >
                                    {/* File icon */}
                                    <div style={{ width: 16, height: 22, display: "flex", alignItems: "center", justifyContent: "center", flexShrink: 0 }}>
                                        <SetiIcon {...getFileIcon(item.label)} size={16} />
                                    </div>
                                    {/* Text */}
                                    <div style={{ marginLeft: 5, overflow: "hidden", display: "flex", flexDirection: "column", flex: 1, minWidth: 0 }}>
                                        <div style={{ display: "flex", alignItems: "center", height: 22 }}>
                                            <HighlightedLabel
                                                text={item.label}
                                                indices={nameIndices}
                                                highlightColor={focused ? colors.listFocusFg : colors.highlight}
                                                baseColor={focused ? colors.listFocusFg : colors.inputFg}
                                            />
                                        </div>
                                    </div>
                                    {/* Path description */}
                                    <span style={{
                                        fontSize: 11, opacity: focused ? 0.8 : 0.7, flexShrink: 0,
                                        maxWidth: 260, overflow: "hidden", textOverflow: "ellipsis",
                                        whiteSpace: "nowrap", marginLeft: 8,
                                    }}>
                                        {pathIndices.length > 0 ? (
                                            <HighlightedLabel
                                                text={item.description}
                                                indices={pathIndices}
                                                highlightColor={focused ? colors.listFocusFg : colors.highlight}
                                                baseColor={focused ? "rgba(255,255,255,0.7)" : colors.description}
                                            />
                                        ) : (
                                            <span>{item.description}</span>
                                        )}
                                    </span>
                                </div>
                            );
                        })
                    )}
                </div>
                <OverlayScrollTarget targetRef={listRef} />
                </div>
            </div>
        </>
    );
}
