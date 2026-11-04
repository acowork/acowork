/**
 * What counts as a **part of a user turn** — the one definition.
 *
 * A part is a row that belongs to the user message above it instead of being a
 * message of its own. Two kinds exist today, and they are deliberately parallel
 * (ADR-107):
 *
 * - **attachments** (ADR-046 §2.5) — things the human dragged in;
 * - **references** (`context_ref`, ADR-107) — context the Runtime supplied that
 *   the human did not type, e.g. the chat background a routed turn came from.
 *   Its body is markdown and is rendered as such; the human's own words are not.
 *
 * ## Why this module exists
 *
 * The rule was written twice — a `ATTACHMENT_TYPES` set in the JSONL loader
 * (`chatStore.convertConversationEntry`, deciding whether to keep `metadata` on
 * the message at all) and a set of predicates in the folder
 * (`messageFolder`, deciding whether to fold the row into the user block) — and
 * the two copies drifted the moment a second part type was added: the loader
 * dropped `context_ref`'s metadata, the folder then saw a metadata-less `system`
 * row, and the reference rendered as a full-width system bubble instead of the
 * card. One registry, both consumers.
 *
 * Both predicates are supplied because the two consumers hold different data:
 * the loader has `(role, metadata)` off the wire, the folder and renderers have
 * a `ChatMessage`.
 */
import type { ChatMessage } from "../../lib/types";

/** `metadata.type` of an attachment entry (ADR-046 §2.5) — the backend
 *  `AttachmentMeta` serde tag. */
export const ATTACHMENT_META_TYPES: ReadonlySet<string> = new Set([
  "file_upload",
  "image_upload",
  "attached_file",
  "attached_selection",
  "attached_folder",
]);

/** `metadata.type` of a reference entry (ADR-107) — the backend
 *  `MessagePartMeta` serde tag. */
export const MESSAGE_REF_META_TYPES: ReadonlySet<string> = new Set(["context_ref"]);

/** Loader-side predicate: does this entry's metadata mark it as a turn part?
 *
 *  Callers use it for the narrower question it answers first — "is this row's
 *  metadata worth keeping on the message at all" — because a part's metadata is
 *  the only thing that tells the renderer which slot the row belongs in. */
export function isTurnPartMeta(
  role: string | undefined,
  meta: Record<string, unknown> | undefined | null,
): boolean {
  if (role !== "system" || !meta) return false;
  const metaType = meta.type;
  if (typeof metaType !== "string") return false;
  return ATTACHMENT_META_TYPES.has(metaType) || MESSAGE_REF_META_TYPES.has(metaType);
}

/** Check if a message is an attachment entry (ADR-046 §2.5). */
export function isAttachmentPart(msg: ChatMessage): boolean {
  if (msg.type !== "system") return false;
  const metaType = msg.metadata?.type as string | undefined;
  return metaType !== undefined && ATTACHMENT_META_TYPES.has(metaType);
}

/** Check if a message is a reference entry (ADR-107). */
export function isMessageRefPart(msg: ChatMessage): boolean {
  if (msg.type !== "system") return false;
  const metaType = msg.metadata?.type as string | undefined;
  return metaType !== undefined && MESSAGE_REF_META_TYPES.has(metaType);
}

/** Any row that folds into the user turn above it. */
export function isUserTurnPart(msg: ChatMessage): boolean {
  return isAttachmentPart(msg) || isMessageRefPart(msg);
}
