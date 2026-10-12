/**
 * InboxPanel — middle-panel renderer for a user↔user inbox thread.
 *
 * Replaces the old full-screen MessagesView once the sidebar `users` entry
 * was retired: the user list is now `UserList` rows inside `AgentList`, and
 * clicking a row sets `useUserChatStore.activePeerId` which makes
 * [AppLayout](../components/layout/AppLayout.tsx) mount this component in
 * the chat pane.
 *
 * Visuals mirror `ChatPanel`'s send + upload composer (textarea + paperclip
 * + send) but skip the model / reasoning / workspace / skill toolbar —
 * user↔user chat has no session config.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { Paperclip, Send, X } from "lucide-react";

import { useAuthStore } from "../stores/authStore";
import { chatIdOf, useUserChatStore } from "../stores/userChatStore";
import { useTranslation } from "../i18n/useTranslation";
import { formatAttachmentSize } from "../lib/user-chat-api";
import { toolbarButton } from "../lib/ui-styles";
import { MessageRow } from "./MessageAttachments";

export function InboxPanel() {
  const { t } = useTranslation();

  const self = useAuthStore((s) => s.account);

  const chats = useUserChatStore((s) => s.chats);
  const activePeerId = useUserChatStore((s) => s.activePeerId);
  const activePeerLabel = useUserChatStore((s) => s.activePeerLabel);
  const messages = useUserChatStore((s) => s.messages);
  const loadingMessages = useUserChatStore((s) => s.loadingMessages);
  const sending = useUserChatStore((s) => s.sending);
  const error = useUserChatStore((s) => s.error);
  const pendingAttachments = useUserChatStore((s) => s.pendingAttachments);
  const uploading = useUserChatStore((s) => s.uploading);
  const removePendingAttachment = useUserChatStore(
    (s) => s.removePendingAttachment,
  );

  const [draft, setDraft] = useState("");
  const fileInput = useRef<HTMLInputElement>(null);
  const bottomRef = useRef<HTMLDivElement>(null);

  // The poll is owned by the store (keyed on the session, not on this pane —
  // see [userChatStore](../stores/userChatStore.ts)). What this component
  // *does* own is the read receipt, because "the user saw these messages" is
  // knowledge only the renderer has.
  useEffect(() => {
    void useUserChatStore.getState().markThreadRead();
  }, [messages, activePeerId]);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ block: "end" });
  }, [messages.length, activePeerId]);

  const submit = useCallback(async () => {
    const body = draft.trim();
    const pending = useUserChatStore.getState().pendingAttachments;
    if (!body && pending.length === 0) return;
    setDraft("");
    // Attachments are consumed by `send`, so a failure must restore the text
    // *and* leave the files queued — hence the id list is passed explicitly.
    const ok = await useUserChatStore
      .getState()
      .send(body, pending.map((a) => a.id));
    if (!ok) setDraft(body);
  }, [draft]);

  const pickFiles = useCallback((files: FileList | null) => {
    if (!files) return;
    const store = useUserChatStore.getState();
    for (const file of Array.from(files)) void store.attach(file);
  }, []);

  const activePeerSummary = activePeerId
    ? chats.find((c) => c.peer_user_id === activePeerId)
    : undefined;
  const activeLabel = activePeerId
    ? (activePeerSummary?.peer_display_name ?? activePeerLabel ?? activePeerId)
    : null;

  if (!activePeerId || !self) {
    return (
      <div className="flex flex-1 items-center justify-center p-8 text-center text-xs text-text-tertiary">
        {t("messages.pick")}
      </div>
    );
  }

  const chatId = chatIdOf(activePeerId, self.user_id);

  return (
    <div className="flex min-w-0 flex-1 flex-col overflow-hidden rounded-xl border border-border-outer bg-chat-body">
      <header className="flex h-9 shrink-0 items-center border-b border-border-divider px-4 text-xs font-semibold text-text-secondary">
        <span className="truncate">{activeLabel}</span>
      </header>

      {error && (
        <div role="alert" className="shrink-0 px-4 py-1 text-11 text-red-500">
          {t("messages.loadFailed")}
        </div>
      )}

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-2">
        {messages.length === 0 && !loadingMessages && (
          <p className="py-2 text-11 text-text-tertiary">
            {t("messages.noMessages")}
          </p>
        )}
        {messages.map((message, i) => {
          // The active thread is always between `self` and `activePeerId`,
          // so `message.from` is one of two ids — branch on which one to
          // pick the avatar props from. ChatSummary.peer_*_avatar was
          // added in the same change as `UserAccount.avatar` propagation
          // to the chat list endpoint, so it lines up with the
          // authenticated user's profile edits without a separate fetch.
          const isOwn = message.from === self.user_id;
          return (
            <MessageRow
              key={`${message.ts}-${i}`}
              message={message}
              own={isOwn}
              youLabel={t("messages.you")}
              userId={self.user_id}
              chatId={chatId}
              ownAvatarUrl={self.avatar}
              ownBuiltinAvatarId={self.builtin_avatar}
              ownDisplayName={self.display_name}
              peerAvatarUrl={activePeerSummary?.peer_avatar ?? null}
              peerBuiltinAvatarId={activePeerSummary?.peer_builtin_avatar ?? null}
              peerDisplayName={activeLabel ?? ""}
            />
          );
        })}
        <div ref={bottomRef} />
      </div>

      <div className="shrink-0">
        {/* Unified input container — mirrors ChatPanel's composer card so the
            user↔user inbox reads as the same primitive as the agent session.
            Wrapping with the rounded-xl surface lets the textarea drop its
            own border (see below) and the paperclip/send buttons collapse to
            the lighter toolbar style. */}
        <div className="mx-6 mb-3 rounded-xl border border-chat-input-border bg-chat-input-bg shadow-[0_1px_3px_rgba(0,0,0,0.04)] dark:shadow-[0_1px_3px_rgba(0,0,0,0.4)]">
          {(pendingAttachments.length > 0 || uploading.length > 0) && (
            <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2">
              {pendingAttachments.map((a) => (
                <span
                  key={a.id}
                  className="flex items-center gap-1 rounded border border-border-outer bg-modal-surface px-1.5 py-0.5 text-10 text-text-secondary"
                >
                  <span className="max-w-40 truncate">{a.filename}</span>
                  <span className="text-text-tertiary">
                    {formatAttachmentSize(a.size)}
                  </span>
                  <button
                    type="button"
                    onClick={() => removePendingAttachment(a.id)}
                    aria-label={`${t("messages.removeAttachment")}: ${a.filename}`}
                    title={t("messages.removeAttachment")}
                    className="ml-0.5 text-text-tertiary hover:text-text-secondary"
                  >
                    <X className="h-3 w-3" />
                  </button>
                </span>
              ))}
              {uploading.map((name) => (
                <span
                  key={name}
                  className="flex items-center gap-1 rounded border border-border-outer bg-modal-surface px-1.5 py-0.5 text-10 text-text-tertiary"
                >
                  <span className="max-w-40 truncate">{name}</span>
                  <span>{t("messages.uploading")}</span>
                </span>
              ))}
            </div>
          )}
          <div className="flex items-end gap-2 px-3 pb-2 pt-1">
            <input
              ref={fileInput}
              type="file"
              multiple
              className="hidden"
              onChange={(e) => {
                pickFiles(e.target.files);
                // Reset so picking the same file twice fires again.
                e.target.value = "";
              }}
            />
            <textarea
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  void submit();
                }
              }}
              placeholder={t("messages.placeholder")}
              aria-label={t("messages.placeholder")}
              disabled={sending}
              className="w-full resize-none border-0 bg-transparent p-3 pb-2 outline-none placeholder:text-text-disabled disabled:cursor-not-allowed disabled:opacity-50 max-h-48 overflow-y-auto min-h-[5rem]"
              style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
            />
          </div>
          {/* Toolbar row — matches ChatPanel's two-row layout: textarea on
              top, action buttons grouped at the bottom right. Inbox has no
              session controls (model / workspace / skills), so the left
              half of the toolbar is empty space. */}
          <div className="flex items-center justify-between gap-2 px-3 pb-2">
            <div />
            <div className="flex shrink-0 items-center gap-1">
              <button
                type="button"
                onClick={() => fileInput.current?.click()}
                aria-label={t("messages.attach")}
                title={t("messages.attach")}
                disabled={sending}
                className={toolbarButton}
              >
                <Paperclip size={14} />
              </button>
              <button
                type="button"
                onClick={() => void submit()}
                disabled={
                  sending ||
                  (draft.trim().length === 0 && pendingAttachments.length === 0)
                }
                aria-label={t("messages.send")}
                title={t("messages.send")}
                className="rounded-md p-1.5 text-text-tertiary hover:bg-zinc-200 disabled:opacity-50 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200"
              >
                <Send size={16} />
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}