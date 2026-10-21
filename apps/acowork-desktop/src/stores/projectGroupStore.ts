/**
 * Group-surface store — the sidebar's project-group section AND the
 * user↔user DM section (design 27 §4.4 / §5.4.4, ADR-098 P0).
 *
 * Both are just `chat_sessions` conversations rendered by the shared
 * `GroupChatPanel` surface: a `project_group` is a project's channel, a
 * `user_user` pair is the smallest possible group (two people). This store
 * mirrors the CURRENT USER's session list (`GET /api/chat-sessions/{me}`,
 * split by `kind`) and owns which session the middle pane shows. Message
 * windows / sends / history search are handled by `chatSessionStore` — the
 * group AND the DM surface are the same Chat-session pipeline.
 *
 * `activeGroupId` is therefore the single "active group-ish session id" — a
 * project group OR a DM. `openDm(peer)` resolves (idempotently creating) the
 * pair session through `GET /default?kind=user_user` and selects it, which is
 * what lets the DM reach `/api/chat-sessions/{sid}/search` for chat history.
 *
 * Mutual exclusion with an agent selection is the CALL SITE's job (the
 * sidebar row handlers import both stores); this store imports neither, so no
 * store-level cycle exists.
 *
 * Local mode (no account) has no user service, so there are no groups or DMs:
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

/** Admin cross-account list (`GET /api/chat-sessions`) — every project group
 *  in the deployment, including groups the admin never joined. The server
 *  already filters to `project_group` and gates on the admin role. */
async function fetchAllGroupSessions(): Promise<ChatSessionInfo[]> {
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(`${getGatewayUrl()}/api/chat-sessions`, {
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  });
  if (!resp.ok) throw new Error(`GET /api/chat-sessions (admin) -> ${resp.status}`);
  return resp.json();
}

/** Resolve (idempotently creating) the caller's `user_user` pair session with
 *  `peer` — the DM twin of the Agent page's `default?pair=<agent>` (§5.4.4).
 *  Returns the full session envelope so the caller can select it without a
 *  second list read. */
async function fetchDmSession(peer: string): Promise<ChatSessionInfo> {
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(
    `${getGatewayUrl()}/api/chat-sessions/default?pair=${encodeURIComponent(peer)}&kind=user_user`,
    { headers: token ? { Authorization: `Bearer ${token}` } : {} },
  );
  if (!resp.ok) throw new Error(`GET /api/chat-sessions/default?kind=user_user -> ${resp.status}`);
  return resp.json();
}

/** Authed JSON request against the Gateway chat-surface API. The app-wide
 *  interceptor is not in play here (this store calls `fetch` directly like the
 *  reads above), so the bearer is attached by hand. Throws on a non-2xx so the
 *  caller (a dialog) can surface the failure. */
async function authRequest<T>(method: string, path: string, body?: unknown): Promise<T> {
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(`${getGatewayUrl()}${path}`, {
    method,
    headers: {
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...(body !== undefined ? { "content-type": "application/json" } : {}),
    },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  if (!resp.ok) throw new Error(`${method} ${path} -> ${resp.status}`);
  return (await resp.json()) as T;
}

/** The peer id of a `user_user` session from `me`'s point of view — the
 *  participant that is not the caller. `null` if the shape is unexpected. */
export function dmPeerOf(session: ChatSessionInfo, me: string): string | null {
  return session.participants?.find((p) => p.id !== me)?.id ?? null;
}

interface ProjectGroupStore {
  /** The caller's group chats — `project_group` AND user-created `user_group`
   *  rows — newest activity first. Rendered by the same `GroupChatPanel`. */
  groups: ChatSessionInfo[];
  /** The caller's `user_user` DM sessions, newest activity first. */
  dms: ChatSessionInfo[];
  /** Session id of the group / DM shown in the middle pane. */
  activeGroupId: string | null;
  fetchGroups: () => Promise<void>;
  /** Open a group in the middle pane. The caller clears any agent /
   *  inbox selection first (see `AgentList`). */
  selectGroup: (sessionId: string) => void;
  /** Resolve / create and open the 1:1 DM session with `peer`. */
  openDm: (peerId: string) => Promise<void>;
  /** Create a `user_group` from `title` + `memberIds` (self is added
   *  automatically), open it, and return its session id. Throws on failure so
   *  the New-group dialog can report it. */
  createGroup: (title: string, memberIds: string[]) => Promise<string>;
  /** Invite `ids` to an existing `user_group` (any member may). Syncs the
   *  local roster from the response; peers are refreshed by the doorbell. */
  addMembers: (sessionId: string, ids: string[]) => Promise<void>;
  /** Remove `pid` from a `user_group` (or leave it when `pid` is self). */
  removeMember: (sessionId: string, pid: string) => Promise<void>;
  clearSelection: () => void;
}

const byActivity = (a: ChatSessionInfo, b: ChatSessionInfo) =>
  b.last_active_at - a.last_active_at;

/** Union two session lists by `session_id`, keeping the first list's entry when
 *  an id appears in both (the caller's own row wins over the admin cross-list,
 *  which lacks the caller's unread / preview context). */
function mergeById(primary: ChatSessionInfo[], extra: ChatSessionInfo[]): ChatSessionInfo[] {
  const seen = new Set(primary.map((s) => s.session_id));
  return [...primary, ...extra.filter((s) => !seen.has(s.session_id))];
}

export const useProjectGroupStore = create<ProjectGroupStore>((set, get) => ({
  groups: [],
  dms: [],
  activeGroupId: null,

  fetchGroups: async () => {
    const { account } = useAuthStore.getState();
    const me = account?.user_id;
    if (!me) {
      set({ groups: [], dms: [] });
      return;
    }
    try {
      const own = await fetchSessions(me);
      const dms = own.filter((s) => s.kind === "user_user").sort(byActivity);
      // Groups = the two "place you talk" kinds: a project's channel and a
      // user-created `user_group`. Both render through `GroupChatPanel`; the
      // `kind` tags the row icon and the member panel's governance path.
      const ownGroups = own.filter((s) => s.kind === "project_group" || s.kind === "user_group");
      // Admin sees every project group (cross-account, read-only) on top of
      // their own rows; a plain user sees only the groups they belong to. DMs
      // are always the caller's own, so they come from the `own` read either
      // way (the admin endpoint returns `project_group` only).
      const groups =
        account?.role === "admin"
          ? mergeById(ownGroups, await fetchAllGroupSessions())
          : ownGroups;
      set({ groups: groups.sort(byActivity), dms });
    } catch (e) {
      // The sidebar sections are best-effort: a user-service blip must not
      // clear a list that is probably still accurate.
      console.warn("[projectGroupStore] fetchGroups failed:", e);
    }
  },

  selectGroup: (sessionId) => {
    set({ activeGroupId: sessionId });
    void useChatSessionStore.getState().openGroup(sessionId);
  },

  openDm: async (peerId) => {
    const me = useAuthStore.getState().account?.user_id;
    if (!me) return;
    try {
      const s = await fetchDmSession(peerId);
      set((st) => {
        const rest = st.dms.filter((d) => d.session_id !== s.session_id);
        return { dms: [s, ...rest].sort(byActivity) };
      });
      get().selectGroup(s.session_id);
    } catch (e) {
      console.warn("[projectGroupStore] openDm failed:", e);
    }
  },

  clearSelection: () => set({ activeGroupId: null }),

  createGroup: async (title, memberIds) => {
    const me = useAuthStore.getState().account?.user_id;
    if (!me) throw new Error("no authenticated user");
    // Self must be in the roster (the server refuses a session the caller
    // isn't part of), and ids dedup so a re-pick doesn't send duplicates.
    const participants = Array.from(new Set([me, ...memberIds.filter((id) => id && id !== me)]));
    const s = await authRequest<ChatSessionInfo>("POST", "/api/chat-sessions", {
      kind: "user_group",
      participants,
      title,
    });
    set((st) => ({
      groups: [s, ...st.groups.filter((g) => g.session_id !== s.session_id)].sort(byActivity),
    }));
    get().selectGroup(s.session_id);
    return s.session_id;
  },

  addMembers: async (sessionId, ids) => {
    if (ids.length === 0) return;
    const s = await authRequest<ChatSessionInfo>(
      "POST",
      `/api/chat-sessions/${encodeURIComponent(sessionId)}/participants`,
      { participant_ids: ids },
    );
    set((st) => ({
      groups: st.groups.map((g) =>
        g.session_id === sessionId ? { ...g, participants: s.participants } : g,
      ),
    }));
  },

  removeMember: async (sessionId, pid) => {
    const me = useAuthStore.getState().account?.user_id;
    const s = await authRequest<ChatSessionInfo>(
      "DELETE",
      `/api/chat-sessions/${encodeURIComponent(sessionId)}/participants/${encodeURIComponent(pid)}`,
    );
    if (me && pid === me) {
      // We left the group: drop the row and release the pane if it was open —
      // the server no longer lists the session for a non-participant.
      set((st) => ({
        groups: st.groups.filter((g) => g.session_id !== sessionId),
        activeGroupId: st.activeGroupId === sessionId ? null : st.activeGroupId,
      }));
      return;
    }
    set((st) => ({
      groups: st.groups.map((g) =>
        g.session_id === sessionId ? { ...g, participants: s.participants } : g,
      ),
    }));
  },
}));
