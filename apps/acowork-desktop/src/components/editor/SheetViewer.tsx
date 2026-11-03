/**
 * SheetViewer — workspace spreadsheet preview (xlsx / xls / csv).
 *
 * Bytes come from the blob path (same authenticated read as every other
 * viewer); SheetJS CE parses them in the webview and the grid is rendered
 * through `VirtualRowList` so a 200k-row sheet only ever mounts the
 * viewport. Deliberate degradations, accepted by the preview plan:
 *
 *   - merged cells are NOT reproduced — the top-left value shows and the
 *     covered cells read empty (SheetJS CE gives no merge fidelity without
 *     a style build; the raw value is the honest fallback);
 *   - formatting beyond text (fonts, colours) is dropped; dates come out
 *     as SheetJS renders them (`raw: false`);
 *   - sheets above the cell budget are previewed only up to
 *     `MAX_PREVIEW_CELLS` worth of rows, with a visible banner.
 *
 * The `xlsx` module is dynamically imported on first parse so its ~900 KB
 * never enters the main bundle (this component is lazy too).
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { AlertCircle } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useWorkspaceFileBlob } from "../../hooks/useWorkspaceFileBlob";
import { VirtualRowList } from "../common/VirtualRowList";
import { DocumentViewerFrame, type ViewerFileProps } from "./DocumentViewerFrame";

/** Row height budget for the virtualizer — one text line, fixed. */
const ROW_HEIGHT_PX = 26;
/** Cell-count ceiling before the "first N rows only" degradation kicks in. */
const MAX_PREVIEW_CELLS = 500_000;
/** Column width clamp (chars are estimated, not measured). */
const MIN_COL_PX = 64;
const MAX_COL_PX = 320;
const CHAR_PX = 7;

type SheetRow = (string | number | boolean | null | undefined)[];

interface ParsedSheet {
    name: string;
    rows: SheetRow[];
    colWidths: number[];
    /** Rows dropped by the cell budget (0 = full preview). */
    truncatedBy: number;
}

function cellText(cell: unknown): string {
    if (cell === null || cell === undefined) return "";
    return String(cell);
}

/** Column widths from a bounded sample — measuring every row to lay out
 *  a 500k-row grid would cost more than the grid itself. */
function computeColWidths(rows: SheetRow[]): number[] {
    const widths: number[] = [];
    const sample = rows.slice(0, 200);
    for (const row of sample) {
        for (let c = 0; c < row.length; c++) {
            const len = cellText(row[c]).length;
            const px = Math.min(MAX_COL_PX, Math.max(MIN_COL_PX, (len + 2) * CHAR_PX));
            widths[c] = Math.max(widths[c] ?? MIN_COL_PX, px);
        }
    }
    return widths;
}

export default function SheetViewer(props: ViewerFileProps) {
    const { t } = useTranslation();
    const state = useWorkspaceFileBlob(
        { agentId: props.agentId, workspaceId: props.workspaceId, relPath: props.relPath },
        { mode: "blob", reloadSeq: props.previewSeq },
    );
    const [sheets, setSheets] = useState<ParsedSheet[]>([]);
    const [activeSheet, setActiveSheet] = useState(0);
    const [parseError, setParseError] = useState<string | null>(null);
    const scrollRef = useRef<HTMLDivElement | null>(null);

    // Re-parse whenever the blob (or a refresh bump) lands.
    useEffect(() => {
        if (!state.blob) return;
        let cancelled = false;
        setParseError(null);
        setActiveSheet(0);
        (async () => {
            const XLSX = await import("xlsx");
            const isCsv = /\.csv$/i.test(props.fileName);
            const wb = isCsv
                ? XLSX.read(await state.blob!.text(), { type: "string" })
                : XLSX.read(await state.blob!.arrayBuffer(), { type: "array" });
            if (cancelled) return;
            const parsed: ParsedSheet[] = wb.SheetNames.map((name) => {
                const ws = wb.Sheets[name];
                // raw:false → formatted display strings (dates / numbers).
                const all = XLSX.utils.sheet_to_json<SheetRow>(ws, {
                    header: 1,
                    raw: false,
                    defval: "",
                });
                const cols = all.reduce((m, r) => Math.max(m, r.length), 1);
                const cap = Math.max(1000, Math.floor(MAX_PREVIEW_CELLS / cols));
                const rows = all.length > cap ? all.slice(0, cap) : all;
                return {
                    name,
                    rows,
                    colWidths: computeColWidths(rows),
                    truncatedBy: all.length - rows.length,
                };
            });
            setSheets(parsed);
        })().catch((e) => {
            if (!cancelled) setParseError(String(e));
        });
        return () => {
            cancelled = true;
        };
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [state.blob, props.previewSeq]);

    const sheet = sheets[activeSheet] ?? null;
    const totalWidth = useMemo(
        () => (sheet ? sheet.colWidths.reduce((a, b) => a + b, 0) : 0),
        [sheet],
    );

    const body = parseError ? (
        <div className="flex h-full flex-col items-center justify-center gap-2 text-xs text-text-tertiary">
            <AlertCircle className="h-4 w-4" />
            {t("fileEditor.sheetViewer.parseFailed")}
            <span className="max-w-[80%] truncate font-mono text-10">{parseError}</span>
        </div>
    ) : sheet ? (
        <div className="flex h-full flex-col">
            {sheets.length > 1 && (
                <div className="flex shrink-0 gap-1 overflow-x-auto border-b border-border-divider px-2 py-1">
                    {sheets.map((s, i) => (
                        <button
                            key={s.name}
                            type="button"
                            onClick={() => setActiveSheet(i)}
                            className={
                                i === activeSheet
                                    ? "rounded px-2 py-0.5 text-xs font-medium bg-hover text-text"
                                    : "rounded px-2 py-0.5 text-xs text-text-tertiary hover:bg-hover"
                            }
                        >
                            {s.name}
                        </button>
                    ))}
                </div>
            )}
            {sheet.truncatedBy > 0 && (
                <div className="shrink-0 border-b border-amber-300/60 bg-amber-50/95 px-3 py-1 text-11 text-amber-900 dark:bg-amber-950/60 dark:text-amber-100">
                    {t("fileEditor.sheetViewer.tooLarge", { rows: sheet.rows.length })}
                </div>
            )}
            <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto">
                <VirtualRowList<SheetRow>
                    items={sheet.rows}
                    keyOf={(_row, index) => `${props.relPath}#${activeSheet}:${index}`}
                    scrollContainerRef={scrollRef}
                    estimateSize={() => ROW_HEIGHT_PX}
                    rowStyle={() => ({ width: totalWidth })}
                    renderItem={(row, index) => (
                        <div
                            className={`flex ${index === 0 ? "font-medium bg-input" : ""}`}
                            style={{ height: ROW_HEIGHT_PX }}
                        >
                            {row.map((cell, c) => (
                                <div
                                    key={c}
                                    className="truncate border-r border-b border-border-divider px-2 text-11 leading-[24px] text-text"
                                    style={{ width: sheet.colWidths[c] ?? MIN_COL_PX }}
                                    title={cellText(cell)}
                                >
                                    {cellText(cell)}
                                </div>
                            ))}
                        </div>
                    )}
                />
            </div>
        </div>
    ) : null;

    return (
        <DocumentViewerFrame file={props} state={state}>
            {body}
        </DocumentViewerFrame>
    );
}
