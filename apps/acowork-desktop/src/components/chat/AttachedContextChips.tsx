import { FileText, Folder, X, Hash } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useChatSessionStore } from "../../stores/chatSessionStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { openAttachedRef } from "../../lib/openWorkspaceRef";

/** Stable empty array reference to avoid Zustand infinite re-renders */
const EMPTY_CTX: never[] = [];

/**
 * Chips showing files/directories/selections attached to the chat input
 * (from right-click "Add to Chat" or programmatic `addAttachedContext`).
 *
 * `surface` picks which composer the chips belong to: the execution view
 * keeps its pending references on the execution session (`chatStore`), the
 * Chat view on the Chat session (`chatSessionStore`). Same list, same UI,
 * different owner — see `lib/addToChat.ts`.
 *
 * Interaction:
 *   - Click the chip body → open the workspace ref in the in-app fileTab
 *     via `openAttachedRef`. `attached_folder` is intentionally a no-op
 *     (fileTab doesn't support folders yet); the chip still renders so
 *     users see what they attached and can remove it.
 *   - Click the X button → drop the chip from the pending attachment list
 *     via `removeAttachedContext`. The X is a real `<button>` (not nested
 *     in the body button) so we don't get an invalid
 *     `button-in-button` DOM structure.
 */
export function AttachedContextChips({ surface = "exec" }: { surface?: "exec" | "chat" }) {
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);

  const chatSessionId = useChatSessionStore((s) =>
    selectedAgentId ? s.byAgent[selectedAgentId]?.chatSessionId ?? null : null,
  );
  const chatAttached = useChatSessionStore((s) =>
    chatSessionId ? s.attachedContext[chatSessionId] ?? EMPTY_CTX : EMPTY_CTX,
  );
  const execAttached = useChatStore((s) => {
    if (!selectedAgentId) return EMPTY_CTX;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return EMPTY_CTX;
    const ss = agent.sessionStates[agent.activeSessionId];
    return ss?.attachedContext ?? EMPTY_CTX;
  });
  const attachedContext = surface === "chat" ? chatAttached : execAttached;

  if (!selectedAgentId || attachedContext.length === 0) return null;

  // Derive the active session's workspace ID once per render — used as
  // the highest-priority candidate root by `resolveAssetAcrossWorkspaces`.
  // The Chat surface has no execution session of its own yet, so there is no
  // priority root and the resolver searches every workspace of the agent.
  const workspaceId =
    surface === "chat"
      ? ""
      : useWorkspaceStore.getState().getSessionWorkspaceId(
          useChatStore.getState().getActiveSessionId(selectedAgentId) ?? "",
        );

  return (
    <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2">
      {attachedContext.map((item) => {
        const handleOpen = () => {
          void openAttachedRef({
            item,
            agentId: selectedAgentId,
            currentWorkspaceId: workspaceId,
          });
        };
        const handleRemove = (e: React.MouseEvent | React.KeyboardEvent) => {
          e.stopPropagation();
          if (surface === "chat") {
            if (chatSessionId) useChatSessionStore.getState().removeAttachedContext(chatSessionId, item.id);
            return;
          }
          const sessionId = useChatStore.getState().getActiveSessionId(selectedAgentId);
          if (!sessionId) return;
          useChatStore.getState().removeAttachedContext(selectedAgentId, sessionId, item.id);
        };
        // Folders can't be opened by fileTab yet (silent no-op), so don't
        // pretend they're clickable — keep the body plain and let users
        // remove them via the X button. Files & selections get the full
        // pointer/keyboard affordance.
        const isOpenable = item.type === "file" || item.type === "selection";
        return (
          <div
            key={item.id}
            className="inline-flex items-center gap-1 rounded-md border border-border-outer bg-zinc-50 text-xs text-text-secondary  dark:bg-zinc-800"
          >
            {isOpenable ? (
              /* Clickable chip body — opens the workspace ref in fileTab.
               * `role="button"` + tabIndex lets keyboard users Enter/Space
               * to trigger. */
              <div
                role="button"
                tabIndex={0}
                className="flex cursor-pointer items-center gap-1 rounded-l-md px-2 py-0.5 hover:bg-zinc-100 dark:hover:bg-zinc-700 focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)]/40"
                title={item.absPath}
                onClick={handleOpen}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    handleOpen();
                  }
                }}
              >
                {item.type === "selection" ? (
                  <Hash className="h-3 w-3 shrink-0 text-[var(--color-accent)]" />
                ) : (
                  <FileText className="h-3 w-3 shrink-0 text-text-tertiary" />
                )}
                <span className="max-w-[200px] truncate">{item.name}</span>
                {item.type === "selection" && item.startLine != null && item.endLine != null && (
                  <span className="text-text-tertiary ">
                    {item.startLine}-{item.endLine}
                  </span>
                )}
              </div>
            ) : (
              /* Non-openable (folder): plain body, no click affordance. */
              <div className="flex items-center gap-1 rounded-l-md px-2 py-0.5">
                <Folder className="h-3 w-3 shrink-0 text-amber-500" />
                <span className="max-w-[200px] truncate">{item.name}</span>
              </div>
            )}
            {/* Real <button> for X — outside the clickable div, so we don't
                get a button-in-button DOM structure. */}
            <button
              type="button"
              className="mr-0.5 rounded p-0.5 text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
              onClick={handleRemove}
              aria-label={`Remove ${item.name}`}
            >
              <X className="h-3 w-3" />
            </button>
          </div>
        );
      })}
    </div>
  );
}