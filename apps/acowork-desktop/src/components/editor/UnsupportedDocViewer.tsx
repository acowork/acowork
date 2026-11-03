/**
 * UnsupportedDocViewer — the honest "no in-app renderer for this" card.
 *
 * Reached for legacy binary Office formats (.doc / .ppt) that the
 * preview stack deliberately does not parse. The frame keeps the two
 * actions that work on raw bytes regardless of format — save as and
 * open externally (the latter is the plan's answer for these files) —
 * so the card is guidance, not a dead end.
 */
import { FileQuestion } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { DocumentViewerFrame, type ViewerFileProps } from "./DocumentViewerFrame";

export default function UnsupportedDocViewer(props: ViewerFileProps) {
    const { t } = useTranslation();
    return (
        <DocumentViewerFrame file={props} bypassFeed>
            <div className="flex h-full flex-col items-center justify-center gap-2 p-6 text-center text-xs text-text-tertiary">
                <FileQuestion className="h-6 w-6" />
                <span>{t("fileEditor.unsupported.formatHint", { name: props.fileName })}</span>
                <span>{t("fileEditor.unsupported.useExternal")}</span>
            </div>
        </DocumentViewerFrame>
    );
}
