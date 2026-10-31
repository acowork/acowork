/**
 * Open a file that hangs off an execution-surface message.
 *
 * The twin of `lib/openChatAttachment.ts`, and the reason both exist is that
 * they address different stores: a chat attachment lives in the user service
 * under its chat session, while an upload made from an execution composer
 * lives under the agent (`/api/agents/{id}/files/{docId}`). One dispatcher per
 * address space, each owning its own routing — a single function taking
 * "either kind of id" would have to guess which namespace a number belongs to.
 *
 * Scope: the `image_upload` arm. The execution surface renders its other arms
 * as `DocumentChip` / workspace-ref chips, which have no blob behind them on
 * this side (a workspace ref is opened through `openAttachedRef`, and a
 * `file_upload` chip is decorative today). Adding text-preview and save branches
 * here would be a table with rows no caller can reach — when that arm gains a
 * click, the branch goes in this file, not in the component.
 *
 * The click decision lives here rather than in the row component for the same
 * reason it lives in `openChatAttachment`: what a click *means* is business
 * logic, and a presentational row that also owns it cannot be reused or tested
 * without mounting the transcript.
 *
 * Every branch degrades visibly — see `openChatAttachment` for why a silent
 * click is the worse failure.
 */
import i18n from "../i18n";
import { showToast } from "../components/common/ToastProvider";
import { openImageLightbox } from "../components/common/ImageLightbox";
import { log } from "./logger";
import { fetchAgentFileBlob, saveAgentFile } from "./agentFile";

/** What the row passes: the metadata its own system entry carries. */
export interface ExecImageAttachment {
  agentId: string;
  documentId: string;
  filename: string;
}

/**
 * Show one execution-surface image at full size. Never throws.
 *
 * `blob` is the bytes the caller already holds (a transcript thumbnail has
 * fetched them once already — the agent-file route serves no thumbnail
 * sidecar, so those bytes *are* the full image). Passing them makes the open
 * a zero-network action; omitting them fetches once, which is what a caller
 * without a live thumbnail does.
 *
 * The object URL is created here and handed back through `onClose`: the
 * lightbox is its only consumer, and a full-size URL that outlives the view
 * is one per image the user ever opened.
 */
export async function openExecImageAttachment(
  attachment: ExecImageAttachment,
  blob?: Blob,
): Promise<void> {
  if (!attachment?.agentId || !attachment?.documentId) return;

  try {
    const bytes = blob ?? (await fetchAgentFileBlob(attachment.agentId, attachment.documentId));
    const src = URL.createObjectURL(bytes);
    openImageLightbox({
      src,
      filename: attachment.filename,
      onDownload: () =>
        void saveAgentFile(
          { agentId: attachment.agentId, documentId: attachment.documentId },
          attachment.filename,
        ),
      onClose: () => URL.revokeObjectURL(src),
    });
  } catch (e) {
    log.warn("[openExecAttachment] failed:", attachment.documentId, e);
    showToast({
      type: "error",
      message: i18n.t("attachment.openFailed", { filename: attachment.filename }),
    });
  }
}
