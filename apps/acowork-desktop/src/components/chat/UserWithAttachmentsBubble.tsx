/**
 * ADR-046 §2.5: User message bubble with attached items.
 *
 * Renders a user text message with its associated attachment chips
 * (file_upload, image_upload, attached_file, attached_selection,
 * attached_folder) in a single right-aligned block with the user avatar.
 *
 * This is the rendering counterpart of the `user_with_parts`
 * block type produced by `foldMessages`.
 */
import { useCallback } from "react";
import { UserAvatar } from "../common/UserAvatar";
import { AttachmentChipRow } from "./AttachmentChipRow";
import type { AttachedItem, ChatMessage } from "../../lib/types";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { openAttachedRef } from "../../lib/openWorkspaceRef";
import { formatBubbleTime } from "../../lib/formatTime";
import { bubbleMetaAnchor } from "./bubbleMetaAnchor";
import { MessageRefBlock } from "./MessageRefBlock";
import { isAttachmentPart, isMessageRefPart } from "./messageFolder";

export interface UserWithAttachmentsBubbleProps {
  /** The user text message (items[0] of the block). */
  userMessage: ChatMessage;
  /** The turn's message parts (items[1..] of the block): attachments
   *  (ADR-046) and references (ADR-107), in log order. Split here by
   *  metadata type — the block is one slot per kind, not one per row. */
  parts: ChatMessage[];
  /** User display name. */
  liveUserName: string | null | undefined;
  /** User avatar URL. */
  liveUserAvatarUrl: string | null | undefined;
  /** User builtin avatar ID. */
  liveUserBuiltinAvatarId: string | null | undefined;
  /** Current session ID (for remove handler). */
  currentSessionId: string;
  /** Agent ID (for image blob fetch). */
  agentId: string | null | undefined;
}

/** Reconstruct an `AttachedItem` from a system message's metadata. */
function metadataToAttachedItem(msg: ChatMessage): AttachedItem | null {
  const meta = msg.metadata;
  if (!meta) return null;
  const metaType = meta.type as string;

  switch (metaType) {
    case "file_upload":
      return {
        type: "file_upload",
        documentId: (meta.document_id as string) ?? (meta.documentId as string) ?? "",
        filename: (meta.filename as string) ?? "",
        format: (meta.format as string) ?? "",
        sizeBytes: (meta.size_bytes as number) ?? (meta.sizeBytes as number) ?? 0,
        clientId: (meta.client_id as string) ?? (meta.clientId as string) ?? undefined,
      };
    case "image_upload":
      return {
        type: "image_upload",
        documentId: (meta.document_id as string) ?? (meta.documentId as string) ?? "",
        filename: (meta.filename as string) ?? "",
        format: (meta.format as string) ?? "",
        sizeBytes: (meta.size_bytes as number) ?? (meta.sizeBytes as number) ?? 0,
        width: typeof meta.width === "number" ? meta.width : undefined,
        height: typeof meta.height === "number" ? meta.height : undefined,
        clientId: (meta.client_id as string) ?? (meta.clientId as string) ?? undefined,
      };
    case "attached_file":
      return {
        type: "attached_file",
        absPath: (meta.abs_path as string) ?? (meta.absPath as string) ?? "",
        name: (meta.name as string) ?? "",
        clientId: (meta.client_id as string) ?? (meta.clientId as string) ?? undefined,
      };
    case "attached_selection":
      return {
        type: "attached_selection",
        absPath: (meta.abs_path as string) ?? (meta.absPath as string) ?? "",
        name: (meta.name as string) ?? "",
        startLine: (meta.start_line as number) ?? (meta.startLine as number) ?? 1,
        endLine: (meta.end_line as number) ?? (meta.endLine as number) ?? 1,
        clientId: (meta.client_id as string) ?? (meta.clientId as string) ?? undefined,
      };
    case "attached_folder":
      return {
        type: "attached_folder",
        absPath: (meta.abs_path as string) ?? (meta.absPath as string) ?? "",
        name: (meta.name as string) ?? "",
        clientId: (meta.client_id as string) ?? (meta.clientId as string) ?? undefined,
      };
    default:
      return null;
  }
}

export function UserWithAttachmentsBubble({
  userMessage,
  parts,
  liveUserName,
  liveUserAvatarUrl,
  liveUserBuiltinAvatarId,
  currentSessionId,
  agentId,
}: UserWithAttachmentsBubbleProps) {
  const removeMessageAttachment = useChatStore((s) => s.removeMessageAttachment);

  const handleChipRemove = useCallback(
    (_item: AttachedItem) => {
      removeMessageAttachment(agentId ?? "", currentSessionId, userMessage.id);
    },
    [agentId, currentSessionId, userMessage.id, removeMessageAttachment],
  );

  const handleChipClick = useCallback(
    (item: AttachedItem) => {
      // Open workspace refs (attached_file / attached_selection) in the
      // in-app fileTab. Matches the handler in MessageBubble.tsx.
      const agentIdFromStore = useAgentStore.getState().selectedAgentId;
      if (!agentIdFromStore) return;
      const workspaceId = useWorkspaceStore
        .getState()
        .getSessionWorkspaceId(currentSessionId);
      void openAttachedRef({
        item,
        agentId: agentIdFromStore,
        currentWorkspaceId: workspaceId,
      });
    },
    [currentSessionId],
  );

  const fontSizeStyle: React.CSSProperties = { fontSize: "var(--ui-font-size, 0.875rem)" };

  const attachments = parts.filter(isAttachmentPart);
  const refs = parts.filter(isMessageRefPart);

  return (
    <div className="flex items-start justify-end gap-2">
      <div className="min-w-0 flex-1 flex flex-col items-end">
        {liveUserName && (
          <span className="mt-[2px] text-xs text-text-tertiary ">
            {liveUserName}
          </span>
        )}

        {/* Attachment chips — rendered above the text bubble */}
        {attachments.length > 0 && (
          <div className="mt-2 flex flex-col items-end gap-1">
            {attachments.map((attMsg) => {
              const item = metadataToAttachedItem(attMsg);
              if (!item) return null;
              return (
                <AttachmentChipRow
                  key={attMsg.id}
                  item={item}
                  agentId={agentId}
                  compact
                  onChipClick={handleChipClick}
                  onRemove={handleChipRemove}
                  pending={!!attMsg._isOptimistic}
                />
              );
            })}
          </div>
        )}

        {/* User text bubble. The timestamp is rendered as a sibling
            BELOW the bubble (not absolutely positioned above) so it
            can never collide with the username row or attachment
            chips. Hover behaviour is preserved: the timestamp
            reveals with an opacity fade when the wrapper is hovered. The
            bubble and timestamp share a column with `max-w-[85%]`, anchored
            by `bubbleMetaAnchor("own")` — the human's own meta block hugs the
            bubble's RIGHT edge, the side the bubble is on. It used to be
            `items-start`, which parked this side's timestamp under the
            bubble's LEFT edge (`bubbleMetaAnchor.ts` records the defect). */}
        {userMessage.content && (
          <div
            className={`group mt-[6px] max-w-[85%] flex flex-col ${bubbleMetaAnchor("own")}`}
          >
            <div
              className="w-full rounded-md rounded-br-sm bg-chat-user text-chat-user-text select-text"
              style={fontSizeStyle}
            >
              {/* Uncapped — see MessageBubble's user branch. */}
              <div className="px-4 py-2.5 whitespace-pre-wrap break-words">
                {userMessage.content}
              </div>
            </div>
            {userMessage.timestamp && (
              <span className="mt-1 text-10 text-text-tertiary  opacity-0 transition-opacity group-hover:opacity-100">
                {formatBubbleTime(userMessage.timestamp)}
              </span>
            )}
          </div>
        )}

        {/* Reference parts (ADR-107) — runtime-supplied context that came
            WITH the message rather than from the human. Rendered BELOW the
            bubble because that is the order the turn reads in: the human's
            words lead, the background follows. Constrained like the bubble
            and right-aligned with it so the block still reads as one turn.
            Rendered even when `content` is empty — an attachment-only or
            reference-only turn has no bubble but does have this card. */}
        {refs.length > 0 && (
          <div className="mt-[6px] flex w-full max-w-[85%] flex-col items-stretch">
            {refs.map((refMsg) => (
              <MessageRefBlock
                key={refMsg.id}
                kind={refMsg.metadata?.kind as string | undefined}
                content={refMsg.content}
              />
            ))}
          </div>
        )}
      </div>

      <UserAvatar
        displayName={liveUserName ?? undefined}
        avatarUrl={liveUserAvatarUrl ?? null}
        // Tri-state passthrough — `null` must stay `null` (see MessageBubble).
        builtinAvatarId={liveUserBuiltinAvatarId}
        size={40}
        className="shrink-0 mt-1"
      />
    </div>
  );
}
