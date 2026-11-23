/**
 * Chat-side handle for a visual workspace file (ADR-121). The live canvas
 * lives in the editor pane; this card is a filename + optional thumbnail
 * so the transcript can reopen it without embedding an iframe in the
 * windowed row list.
 */
import { useCallback } from "react";
import { FileText } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useAgentStore } from "../../stores/agentStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useWorkspaceFileBlob } from "../../hooks/useWorkspaceFileBlob";
import { AttachmentCapsule } from "./AttachmentCapsule";
import { previewPngRelPath } from "../editor/visualCanvas";

export interface VisualCanvasCardProps {
  relPath: string;
  sessionId?: string | null;
  /** Bumped when a later screenshot lands so a 404 thumb is retried. */
  reloadSeq?: number;
}

export function VisualCanvasCard({ relPath, sessionId, reloadSeq = 0 }: VisualCanvasCardProps) {
  const { t } = useTranslation();
  const agentId = useAgentStore((s) => s.selectedAgentId);
  const workspaceId = useWorkspaceStore((s) =>
    sessionId ? s.getSessionWorkspaceId(sessionId) : "",
  );
  const fileName = relPath.split("/").pop() || relPath;
  const canOpen = !!agentId && !!workspaceId;
  const thumbRef = canOpen
    ? { agentId, workspaceId, relPath: previewPngRelPath(relPath) }
    : null;
  const { blobUrl } = useWorkspaceFileBlob(thumbRef, {
    mode: "url",
    reloadSeq,
    enabled: canOpen,
  });

  const handleOpen = useCallback(() => {
    if (!agentId || !workspaceId) return;
    void useFileEditorStore.getState().openPreview(agentId, workspaceId, relPath);
  }, [agentId, workspaceId, relPath]);

  return (
    <AttachmentCapsule
      onClick={canOpen ? handleOpen : undefined}
      title={relPath}
      compact={false}
      ariaLabel={t("fileEditor.visualCanvas.open")}
      data-testid="visual-canvas-card"
    >
      {blobUrl ? (
        <img
          src={blobUrl}
          alt=""
          className="h-10 w-10 shrink-0 rounded object-cover"
        />
      ) : (
        <FileText className="h-4 w-4 shrink-0 text-text-tertiary" />
      )}
      <span className="max-w-[12rem] truncate">{fileName}</span>
    </AttachmentCapsule>
  );
}
