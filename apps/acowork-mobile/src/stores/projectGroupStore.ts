/**
 * Project-group store — the mobile twin of Desktop's `projectGroupStore`
 * (design 27 §4.4, ADR-098), reduced to its group half.
 *
 * A group is a `chat_sessions` conversation of kind `project_group` (a
 * project's channel, roster synced from the PM project server-side) or
 * `user_group` (human-founded, roster managed through the member routes).
 * Both render through the same screen; the `kind` only tags the row and the
 * member sheet's governance path.
 *
 * The DM half of the Desktop store is deliberately NOT ported: mobile's
 * 1:1 human thread is the `userChatStore` surface (flat message log with a
 * server-side read cursor), and the inbox already lists it as 联系人. Two
 * surfaces for the same pair would mean two truths about read state.
 *
 * Freshness rides the one doorbell the app already subscribes
 * (`acowork/users/{me}/chat/changed`, wired in main.tsx): content-free
 * signal → HTTP re-read. The list is refetched when the inbox opens and on
 * every wake; there is no local patching of rows.
 *
 * Local mode (no account) has no user service, so there are no groups: the
 * section hides itself and `fetchGroups` resolves empty.
 */

import { create } from 'zustand'
import { fetchMyChatSessions } from '../lib/api'
import { useAuthStore } from './authStore'
import type { ChatSessionInfo } from '../lib/types'

/** The two "place you talk" kinds (design 27 §4.4). Everything else in the
 *  session list — the 1:1 `agent_user` pairs, the `user_user` DM twins — is
 *  owned by another section of the inbox and must not appear here twice. */
const GROUP_KINDS = new Set(['project_group', 'user_group'])

const byActivity = (a: ChatSessionInfo, b: ChatSessionInfo) =>
  b.last_active_at - a.last_active_at

interface ProjectGroupStore {
  /** This account's group sessions, most recent first. */
  groups: ChatSessionInfo[]
  groupsLoaded: boolean
  /** The group currently on screen; null = no group surface open. Set by
   *  the inbox row tap, read by `GroupChatScreen` (the twin of Desktop's
   *  `activeGroupId`, which the middle pane mounts from). */
  activeGroupId: string | null

  /** Re-pull this account's sessions and keep the group kinds. A failure
   *  keeps the previous list: the inbox is best-effort and a user-service
   *  blip must not clear a list that is probably still accurate. */
  fetchGroups(): Promise<void>
  /** The inbox tap path: remember which group the screen shows. The caller
   *  additionally opens the session window (`chatSessionStore.openGroup`). */
  selectGroup(sessionId: string): void
  /** Drop the selection (screen unmount). */
  clearSelection(): void
}

export const useProjectGroupStore = create<ProjectGroupStore>((set) => ({
  groups: [],
  groupsLoaded: false,
  activeGroupId: null,

  fetchGroups: async () => {
    const me = useAuthStore.getState().me?.user_id
    if (!me) {
      set({ groups: [], groupsLoaded: true })
      return
    }
    try {
      const own = await fetchMyChatSessions(me)
      const groups = own.filter((s) => GROUP_KINDS.has(s.kind)).sort(byActivity)
      set({ groups, groupsLoaded: true })
    } catch (e) {
      console.warn('[projectGroupStore] fetchGroups failed:', e)
      set({ groupsLoaded: true })
    }
  },

  selectGroup: (sessionId) => set({ activeGroupId: sessionId }),

  clearSelection: () => set({ activeGroupId: null }),
}))

/** The doorbell reaction: the group list is cheap and whole — re-read it.
 *  (The open transcript is woken separately through `chatSessionStore`.) */
export function wakeGroupChat(): Promise<void> {
  return useProjectGroupStore.getState().fetchGroups()
}
