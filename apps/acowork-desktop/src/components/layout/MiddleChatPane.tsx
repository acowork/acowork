import { AgentPage } from "../chat/AgentPage";
import { GroupChatPanel } from "../chat/GroupChatPanel";
import { EmptyChatPane } from "../chat/EmptyChatPane";

/**
 * The middle pane's view selection for `AppLayout`: an agent session page, an
 * open group / DM transcript, or the empty state.
 *
 * The `key` is the point of this component, not a detail. Both panes own
 * per-view scroll state — each holds a `FollowController` (`useStickToBottom`)
 * and the `useVirtualizer` inside `VirtualRowList`, whose baselines are the
 * scroll offset, the armed jump anchor and the measured row heights. Rendering
 * either one for a DIFFERENT view without a key reuses the instance and applies
 * the previous view's geometry to the new one: an agent switch then parked its
 * transcript mid-list or at the top while a fresh mount (agent startup) always
 * positioned correctly. It is the same rule `ChatPanel` states for its own
 * list — `key={currentScrollKey}` — and the reason `AgentPage` keys
 * `AgentChatView` by chat session.
 *
 * Its own module, rather than an inline ternary in `AppLayout`, so this
 * identity rule stays reviewable and testable without mounting the whole shell.
 */
export function MiddleChatPane({
  /** The selected agent, or null when a group / DM owns the pane. */
  agentId,
  /** The active group or DM session id, or null. */
  groupId,
}: {
  agentId: string | null;
  groupId: string | null;
}) {
  if (agentId) return <AgentPage key={agentId} />;
  if (groupId) return <GroupChatPanel key={groupId} />;
  return <EmptyChatPane />;
}