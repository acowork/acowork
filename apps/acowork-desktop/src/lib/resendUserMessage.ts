/**
 * Resend a user turn's text into the current agent session.
 *
 * Both execution-surface user bubbles offer it — the plain `MessageBubble`
 * and the with-parts `UserWithAttachmentsBubble` — and each of them from two
 * entry points: the right-click menu and the hover meta row. Four call sites
 * that must be ONE behaviour, so none of them owns the logic.
 *
 * It reuses `sendMessage`, which already allocates the message id, performs
 * the optimistic insert, clears the attached items and publishes.
 *
 * Attachments are intentionally dropped: they belonged to the previous send,
 * and carrying them into a fresh one would double-charge the upload pipeline.
 */
import { useAgentStore } from "../stores/agentStore";
import { useChatStore } from "../stores/chatStore";

export function resendUserMessageText(content: string): void {
  const agentId = useAgentStore.getState().selectedAgentId;
  if (!agentId) return;
  if (!content) return;
  void useChatStore.getState().sendMessage(content, agentId);
}
