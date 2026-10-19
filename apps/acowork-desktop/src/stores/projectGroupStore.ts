/**
 * Project-group store — the sidebar's "project group chat" section
 * (design 27 §4.4: one `project_group` Chat session per project,
 * provisioned by PM on project create and backfilled on PM start).
 *
 * The sessions live in the user service's chat domain; this store only
 * mirrors the CURRENT USER's group list (`GET /api/chat-sessions/{me}`
 * filtered to `kind === "project_group"`) and owns which group is shown
 * in the middle pane. Message windows / sends are handled by
 * `chatSessionStore` — the group surface is just another Chat session.
 *
 * Mutual exclusion with an agent selection / inbox peer is the CALL
 * SITE's job (the sidebar row handlers import all three stores); this
 * store deliberately imports neither, so no store-level cycle exists.
 *
 * Local mode (no account) has no user service, so there are no groups:
 * the section hides itself.
 */

import { create } from "zustand";
import { getGatewayUrl } from "../lib/config";
import { useAuthStore } from "./authStore";
import { useChatSessionStore, type ChatSessionInfo } from "./chatSessionStore";

async function fetchSessions(me: string): Promise<ChatSessionInfo[]> {
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(`${getGatewayUrl()}/api/chat-sessions/${encodeURIComponent(me)}`, {
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  });
  if (!resp.ok) throw new Error(`GET /api/chat-sessions/{me} -> ${resp.status}`);
  return resp.json();
}

interface ProjectGroupStore {
  /** The caller's project group chats, newest activity first. */
  groups: ChatSessionInfo[];
  /** Session id of the group shown in the middle pane. */
  activeGroupId: string | null;
  fetchGroups: () => Promise<void>;
  /** Open a group in the middle pane. The caller clears any agent /
   *  inbox selection first (see `AgentList`). */
  selectGroup: (sessionId: string) => void;
  clearSelection: () => void;
}

export const useProjectGroupStore = create<ProjectGroupStore>((set) => ({
  groups: [],
  activeGroupId: null,

  fetchGroups: async () => {
    const me = useAuthStore.getState().account?.user_id;
    if (!me) {
      set({ groups: [] });
      return;
    }
    try {
      const all = await fetchSessions(me);
      const groups = all
        .filter((s) => s.kind === "project_group")
        .sort((a, b) => b.last_active_at - a.last_active_at);
      set({ groups });
    } catch (e) {
      // The sidebar section is best-effort: a user-service blip must not
      // clear a list that is probably still accurate.
      console.warn("[projectGroupStore] fetchGroups failed:", e);
    }
  },

  selectGroup: (sessionId) => {
    set({ activeGroupId: sessionId });
    void useChatSessionStore.getState().openGroup(sessionId);
  },

  clearSelection: () => set({ activeGroupId: null }),
}));
