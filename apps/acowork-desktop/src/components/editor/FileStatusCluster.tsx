import { type LspStatus } from "../../lib/lspUtils";
import { LspIndicator } from "./LspIndicator";
import { useTranslation } from "../../i18n/useTranslation";
import { useEditorStatusStore } from "../../stores/editorStatusStore";

/**
 * Minimal shape of the currently active file that the cluster needs.
 *
 * This is intentionally a structural subset of `OpenFile` (not the full
 * type) so the cluster stays a pure renderer with no store coupling.
 * The wiring up (which fields map to which `OpenFile` fields) lives in
 * `FileEditorPanel` — single source of data-flow.
 */
export interface FileStatusClusterActiveFile {
    fileName: string;
    language?: string;
    mimeType?: string;
    mode: "edit" | "preview";
    kind: "file" | "url" | "diff" | "log" | "attachment" | "doc";
    url?: string;
    relPath: string;
    loading?: boolean;
    /** Agent hosting this file (ADR-055 §6.7 — per-agent LSP relay resolution) */
    agentId?: string;
}

export interface EditorCursorPosition {
    line: number;
    column: number;
}

export interface FileStatusClusterProps {
    /**
     * The currently active file. Cluster renders nothing when `null`
     * or while `loading` — keeps the global-bar layout clean during
     * tab switches.
     */
    activeFile: FileStatusClusterActiveFile | null;
    cursor: EditorCursorPosition;
    selectedCount: number;
    lspEnabled: boolean;
    lspLanguage: string | null;
    lspStatus: LspStatus | null;
    lspStatusMessage: string;
}

/**
 * Renders the three file-status segments — language / LSP / cursor —
 * as the FOOTER of the file editor panel.
 *
 * It used to float in the app-wide bottom status bar, absolutely
 * positioned over the panel's column. The bottom bar is gone, and a
 * floating strip over the content is the wrong shape for a footer that
 * belongs to exactly one panel: it now sits inside the panel's own
 * flex column, so it cannot drift when the panel is resized or when the
 * right panel opens.
 *
 * Layout-pure: props in, DOM out. No store coupling, no positioning math.
 *
 * Visibility rule (single):
 *   - Returns `null` if `activeFile` is absent or still loading —
 *     otherwise the cluster would briefly render stale language /
 *     cursor values during tab switches.
 */
export function FileStatusCluster({
    activeFile,
    cursor,
    selectedCount,
    lspEnabled,
    lspLanguage,
    lspStatus,
    lspStatusMessage,
}: FileStatusClusterProps) {
    const { t } = useTranslation();

    if (!activeFile || activeFile.loading) {
        return null;
    }

    const isEdit = activeFile.mode === "edit" && activeFile.kind === "file";
    const isUrl = activeFile.kind === "url";

    return (
        <div
            className="flex shrink-0 items-center justify-between gap-4 border-t border-border-divider bg-surface/60 px-3 py-px text-11 text-text-tertiary select-none"
        >
            {isEdit ? (
                <>
                    <span className="uppercase truncate min-w-0">
                        {activeFile.language || t("fileStatus.languageFallback")}
                    </span>
                    {lspEnabled && lspLanguage && lspStatus && (
                        <div className="shrink-0">
                            <LspIndicator
                                status={lspStatus}
                                statusMessage={lspStatusMessage}
                                language={lspLanguage}
                                agentId={activeFile.agentId}
                            />
                        </div>
                    )}
                    <span className="truncate min-w-0 text-right">
                        {t("fileStatus.cursorPosition", {
                            line: cursor.line,
                            column: cursor.column,
                        })}
                        {selectedCount > 0
                            ? ` ${t("fileStatus.selectionCount", { count: selectedCount })}`
                            : ""}
                    </span>
                </>
            ) : (
                <>
                    <span className="uppercase truncate min-w-0">
                        {isUrl
                            ? t("fileStatus.previewKind.url")
                            : (activeFile.mimeType || activeFile.language || "")}
                    </span>
                    <span className="truncate min-w-0 text-right">
                        {isUrl
                            ? (() => {
                                  try {
                                      return new URL(activeFile.url || activeFile.relPath).host;
                                  } catch {
                                      return activeFile.url || activeFile.relPath;
                                  }
                              })()
                            : ""}
                    </span>
                </>
            )}
        </div>
    );
}

/**
 * The store-connected footer.
 *
 * `FileStatusCluster` stays a pure renderer (props in / DOM out) so it can
 * be tested and snapshot in isolation. THIS wrapper is what the panel
 * mounts, and it subscribes to the cursor/selection store itself — so a
 * cursor move re-renders the two-line footer, not the whole editor panel.
 * The panel used to hold the subscription for the app-wide bottom bar and
 * paid that re-render on every keystroke-sized selection change.
 */
export function FileStatusFooter(
    props: Omit<FileStatusClusterProps, "cursor" | "selectedCount">,
) {
    const cursor = useEditorStatusStore((s) => s.cursor);
    const selectedCount = useEditorStatusStore((s) => s.selectedCount);
    return <FileStatusCluster {...props} cursor={cursor} selectedCount={selectedCount} />;
}
