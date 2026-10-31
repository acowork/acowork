/**
 * Open a file that hangs off a chat message.
 *
 * The chat surface stores attachment bytes in the user service
 * (`chat-sessions/{sid}/files/`), which is not a workspace path — so this
 * cannot go through `openAttachedRef` (its whole contract is an `absPath`
 * resolved across workspaces, and it refuses uploads on purpose). Two
 * dispatchers, two domains: this one owns chat blobs.
 *
 * Three destinations, chosen from data the row already carries — never from a
 * guess:
 *
 * | condition                        | action                       |
 * |----------------------------------|------------------------------|
 * | `mime` starts with `image/`      | lightbox over the full blob  |
 * | text-readable and ≤ 8 MiB        | read-only Monaco tab         |
 * | anything else                    | save dialog + Rust transfer  |
 *
 * Every branch degrades visibly: a chat attachment can legitimately be gone
 * (a migrated row whose blob predates the store, or an offline Gateway), and
 * a click that silently does nothing is worse than a toast that says so.
 */
import { showToast } from "../components/common/ToastProvider";
import { openImageLightbox } from "../components/common/ImageLightbox";
import i18n from "../i18n";
import {
  canPreviewAttachment,
  downloadChatAttachmentBySession,
} from "./user-chat-api";
import { languageForPath } from "./monacoLanguage";
import { log } from "./logger";
import {
  createChatAttachmentObjectUrl,
  fetchChatAttachmentText,
} from "../stores/chatSessionStore";
import { useFileEditorStore } from "../stores/fileEditorStore";
import type { ChatAttachment } from "./types";

/** What the chip passes: the row's own metadata. */
export type OpenableChatAttachment = Pick<ChatAttachment, "id" | "filename" | "mime" | "size">;

/**
 * Open one chat attachment. Never throws — the click path has no error
 * surface of its own, so failures become a toast.
 */
export async function openChatAttachment(
  chatSessionId: string,
  attachment: OpenableChatAttachment,
): Promise<void> {
  if (!chatSessionId || !attachment?.id) return;

  // The preview gate wants the full `ChatAttachment` shape; `size` is what it
  // actually reads, and a row without it cannot claim to be under the cap.
  const withSize: ChatAttachment = {
    id: attachment.id,
    filename: attachment.filename,
    mime: attachment.mime,
    size: attachment.size,
  };

  try {
    if (attachment.mime.startsWith("image/")) {
      // Owned, not cached: the lightbox is the only consumer of this URL and
      // it hands it back on close. A cached full-size blob would live for the
      // rest of the process — one per image the user ever opened.
      const src = await createChatAttachmentObjectUrl(chatSessionId, attachment.id);
      openImageLightbox({
        src,
        filename: attachment.filename,
        onDownload: () => void save(chatSessionId, withSize),
        onClose: () => URL.revokeObjectURL(src),
      });
      return;
    }

    if (canPreviewAttachment(withSize)) {
      const content = await fetchChatAttachmentText(chatSessionId, attachment.id);
      useFileEditorStore.getState().openAttachmentPreview({
        chatId: chatSessionId,
        attachmentId: attachment.id,
        fileName: attachment.filename,
        content,
        language: languageForPath(attachment.filename),
      });
      return;
    }

    await save(chatSessionId, withSize);
  } catch (e) {
    log.warn("[openChatAttachment] failed:", attachment.id, e);
    showToast({
      type: "error",
      message: i18n.t("attachment.openFailed", { filename: attachment.filename }),
    });
  }
}

/** Save-to-disk branch, shared by the lightbox button and the binary path. */
async function save(
  chatSessionId: string,
  attachment: ChatAttachment,
): Promise<void> {
  await downloadChatAttachmentBySession(chatSessionId, attachment);
}