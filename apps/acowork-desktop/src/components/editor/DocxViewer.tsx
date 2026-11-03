/**
 * DocxViewer — workspace .docx preview via docx-preview.
 *
 * The library re-flowed the OOXML document into paginated HTML matching
 * Word's layout reasonably well; it is dynamically imported (lazy
 * component on top), so its weight never touches the main bundle.
 *
 * Anything the library rejects — password-encrypted files, corrupt
 * archives, .doc binaries mislabeled as .docx — degrades to a card that
 * points at the two header actions (save as / open externally), which
 * work on the raw bytes and therefore on any file.
 */
import { useEffect, useRef, useState } from "react";
import { AlertCircle } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useWorkspaceFileBlob } from "../../hooks/useWorkspaceFileBlob";
import { DocumentViewerFrame, type ViewerFileProps } from "./DocumentViewerFrame";

export default function DocxViewer(props: ViewerFileProps) {
    const { t } = useTranslation();
    const state = useWorkspaceFileBlob(
        { agentId: props.agentId, workspaceId: props.workspaceId, relPath: props.relPath },
        { mode: "blob", reloadSeq: props.previewSeq },
    );
    const containerRef = useRef<HTMLDivElement | null>(null);
    const hostRef = useRef<HTMLDivElement | null>(null);
    const [renderError, setRenderError] = useState<string | null>(null);

    useEffect(() => {
        const el = containerRef.current;
        if (!state.blob || !el) return;
        let cancelled = false;
        setRenderError(null);
        (async () => {
            const { renderAsync } = await import("docx-preview");
            if (cancelled || !containerRef.current) return;
            containerRef.current.innerHTML = "";
            // Default options keep Word's page wrapper (inWrapper) — the
            // document then scrolls like pages, not one infinite column.
            await renderAsync(state.blob, containerRef.current, undefined, {
                inWrapper: true,
                ignoreLastRenderedPageBreak: true,
            });
            fitRenderedPages(containerRef.current, hostRef.current);
        })().catch((e) => {
            if (!cancelled) setRenderError(String(e));
        });
        // The panel is resizable — refit when the scroll host changes width
        // (page base width is measured once and cached, so this is stable).
        const ro = new ResizeObserver(() => {
            fitRenderedPages(containerRef.current, hostRef.current);
        });
        if (hostRef.current) ro.observe(hostRef.current);
        return () => {
            cancelled = true;
            ro.disconnect();
        };
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [state.blob, props.previewSeq]);

    return (
        <DocumentViewerFrame file={props} state={state}>
            {renderError ? (
                <div className="flex h-full flex-col items-center justify-center gap-2 p-6 text-center text-xs text-text-tertiary">
                    <AlertCircle className="h-5 w-5" />
                    <span>{t("fileEditor.docxViewer.renderFailed")}</span>
                    <span className="max-w-[80%] truncate font-mono text-10">{renderError}</span>
                </div>
            ) : (
                <div ref={hostRef} className="h-full overflow-auto bg-zinc-100 dark:bg-zinc-900">
                    <div ref={containerRef} data-testid="docx-preview-container" />
                </div>
            )}
        </DocumentViewerFrame>
    );
}

/**
 * Repair docx-preview's wrapper layout for a narrow preview pane.
 *
 * The library centers pages with `align-items: center` — the classic
 * centered-overflow trap: once a page is wider than the pane, the left
 * half of the overflow (the page's OWN margin, first characters of every
 * line) is clipped beyond scroll reach. `safe center` centers only while
 * it fits; engines without `safe` support reject that assignment and keep
 * the `flex-start` fallback written just before it.
 *
 * Then scale-to-fit: a full A4 page in a 700 px pane is unreadable, so
 * the wrapper gets a `zoom` matching the available width (floored at
 * 0.35 — below that the page is a postage stamp and scrolling is the
 * honest answer). The unzoomed page width is measured once and cached on
 * the wrapper, because `offsetWidth` under an applied `zoom` would feed
 * back into itself on every resize.
 */
export function fitRenderedPages(
    container: HTMLElement | null,
    host: HTMLElement | null,
): void {
    if (!container) return;
    const wrapper = container.querySelector<HTMLElement>(".docx-wrapper");
    if (!wrapper) return;
    wrapper.style.alignItems = "flex-start";
    wrapper.style.alignItems = "safe center";
    const page = wrapper.querySelector<HTMLElement>("section.docx");
    if (!page) return;
    if (!wrapper.dataset.docxPageWidth) {
        const base = page.getBoundingClientRect().width;
        if (base > 0) wrapper.dataset.docxPageWidth = String(base);
    }
    const baseWidth = Number(wrapper.dataset.docxPageWidth ?? 0);
    const available = (host?.clientWidth ?? 0) - 8;
    if (baseWidth <= 0 || available <= 0) return;
    const zoom = baseWidth > available ? Math.max(0.35, available / baseWidth) : 1;
    wrapper.style.zoom = zoom === 1 ? "" : String(zoom);
}
