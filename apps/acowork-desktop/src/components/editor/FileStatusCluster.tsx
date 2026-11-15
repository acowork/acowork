import { type LspStatus } from "../../lib/lspUtils";
import { LspIndicator } from "./LspIndicator";
import { useTranslation } from "../../i18n/useTranslation";

/**
 * Minimal shape of the currently active file that the cluster needs.
 *
 * This is intentionally a structural subset of `OpenFile` (not the full
 * type) so the cluster stays a pure renderer with no store coupling.
 * The wiring up (which fields map to which `OpenFile` fields) lives in
 * `AppLayout` — single source of data-flow.
 */
export interface FileStatusClusterActiveFile {
    fileName: string;
    language?: string;
    mimeType?: string;
    mode: "edit" | "preview";
    kind: "file" | "url" | "diff" | "log" | "attachment";
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

interface FileStatusClusterProps {
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
 * for the active file tab.
 *
 * Designed to be wrapped by an absolutely-positioned container anchored
 * to `useLayoutStore.filePanelBounds` (positioning math is the caller's
 * responsibility — this stays a layout-pure component, easy to test
 * and snapshot).
 *
 * Visual spec mirrors the original per-file bar
 * (`FileEditorPanel.tsx` pre-refactor lines 1540–1572) so the
 * parallel-run period in PR-2 feels unchanged. PR-3 will remove the
 * copy in `FileEditorPanel` entirely.
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
            className="flex items-center justify-between gap-4 rounded-md border border-border-outer/50 bg-surface/80 px-3 py-px text-11 text-text-tertiary select-none "
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
