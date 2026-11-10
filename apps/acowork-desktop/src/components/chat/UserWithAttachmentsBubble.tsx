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
import { AgentAvatar } from "../common/AgentAvatar";
import { ChatMarkdown } from "./ChatMarkdown";
import {
  speakerBubbleClass,
  speakerRendersMarkdown,
  type SpeakerKind,
} from "./speakerBubble";
import { SpeakerBadge } from "./speakerNames";
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
  /**
   * What KIND the session's initiator — the speaker of this row — is. An
   * agent-to-agent run has an Agent on this side, and its words are markdown;
   * the bubble then takes the agent skin and the markdown renderer
   * (`speakerBubble.ts`). Defaults to `"user"`.
   */
  speakerKind?: SpeakerKind;
  /** The initiator's agent instance id when `speakerKind` is `"agent"`
   *  (`AgentAvatar`'s seed), else null. */
  senderAgentId?: string | null;
  /** The initiator agent's package version, for `AgentAvatar`'s cache bust. */
  senderAgentVersion?: string | null;
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
  speakerKind = "user",
  senderAgentId = null,
  senderAgentVersion = null,
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
  // Renderer + skin both follow the SPEAKER, decided once so the two cannot
  // be configured apart (`speakerBubble.ts`).
  const asAgentSpeech = speakerRendersMarkdown(speakerKind);
  // Narrowed once, so the avatar branch below has a non-null instance id.
  const senderAgent = speakerKind === "agent" ? senderAgentId : null;
  // Plan D3: this row's voice. `SpeakerBadge` decides whether there is
  // anything to say (nothing, for an ordinary turn) and says NAMES rather
  // than the ids the record carries — the same pure rule as the plain user
  // bubble, so the two paths cannot disagree.

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
        <SpeakerBadge
          speaker={userMessage.speaker}
          className="mt-[2px] text-xs text-text-tertiary"
        />

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

        {/* One column per turn: the bubble, then its reference parts, then the
            timestamp.

            The timestamp is a sibling BELOW the bubble rather than an
            absolutely positioned element, so it can never collide with the
            username row or the attachment chips; hover reveals it with an
            opacity fade. It is deliberately the **last** child: it keeps its
            height while hidden (`opacity-0`, not `hidden`), so putting a
            hover-only meta line between the words and their background opened
            an empty strip in the middle of the turn — the two looked like
            separate turns. Meta belongs at the end of the turn, not inside it.

            The column is `w-full max-w-[85%]`, anchored by
            `bubbleMetaAnchor("own")`: the human's own meta block hugs the
            bubble's RIGHT edge, the side the bubble is on. It used to be
            `items-start`, which parked this side's timestamp under the bubble's
            LEFT edge (`bubbleMetaAnchor.ts` records the defect).

            Its width comes from the ROW (`w-full` + `max-w-[85%]`), never from
            its children, and the bubble inside is `w-fit`: a column sized by its
            contents let the widest child set everyone's width, so a two-word
            message stretched to the width of its reference card — and stretched
            again when that card expanded. A fixed column with intrinsically
            sized children cannot do that. */}
        {(userMessage.content || refs.length > 0) && (
          <div
            className={`group mt-[6px] w-full max-w-[85%] flex flex-col ${bubbleMetaAnchor("own")}`}
          >
            {userMessage.content && (
              <div
                className={`w-fit max-w-full rounded-md rounded-br-sm ${speakerBubbleClass(speakerKind)} px-4 py-2.5 select-text break-words`}
                style={fontSizeStyle}
              >
                {/* Uncapped — see MessageBubble's user branch. The renderer
                    and the skin follow the SPEAKER, not the role: an Agent
                    initiator's brief is markdown, a person's typed words are
                    literal (`speakerBubble.ts`). */}
                {asAgentSpeech ? (
                  <ChatMarkdown content={userMessage.content} style={fontSizeStyle} />
                ) : (
                  <div className="whitespace-pre-wrap break-words">
                    {userMessage.content}
                  </div>
                )}
              </div>
            )}

            {/* Reference parts (ADR-107) — runtime-supplied context that came
                WITH the message rather than from the human. They sit directly
                under the words and inside the same column, so they hug the
                bubble's right edge and read as one turn: words lead, background
                follows. Rendered even when `content` is empty — an
                attachment-only turn has no bubble but does have this card. */}
            {refs.map((refMsg) => (
              <MessageRefBlock
                key={refMsg.id}
                kind={refMsg.metadata?.kind as string | undefined}
                content={refMsg.content}
              />
            ))}

            {userMessage.timestamp && (
              <span className="mt-1 text-10 text-text-tertiary  opacity-0 transition-opacity group-hover:opacity-100">
                {formatBubbleTime(userMessage.timestamp)}
              </span>
            )}
          </div>
        )}
      </div>

      {senderAgent ? (
        // An Agent initiator is drawn with the agent avatar: its `avatarUrl`
        // is a package-relative path only `AgentAvatar` can resolve (see
        // MessageBubble's user branch for the same rule).
        <AgentAvatar
          agentId={senderAgent}
          displayName={liveUserName ?? undefined}
          avatarUrl={liveUserAvatarUrl ?? null}
          version={senderAgentVersion}
          builtinAvatarId={liveUserBuiltinAvatarId ?? null}
          size={40}
          className="shrink-0 mt-1"
        />
      ) : (
        <UserAvatar
          displayName={liveUserName ?? undefined}
          avatarUrl={liveUserAvatarUrl ?? null}
          // Tri-state passthrough — `null` must stay `null` (see MessageBubble).
          builtinAvatarId={liveUserBuiltinAvatarId}
          size={40}
          className="shrink-0 mt-1"
        />
      )}
    </div>
  );
}
