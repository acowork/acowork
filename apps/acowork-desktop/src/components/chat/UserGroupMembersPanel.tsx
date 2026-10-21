/**
 * UserGroupMembersPanel — the right-hand member surface for a user-created
 * `user_group` (P1/P2).
 *
 * Unlike a `project_group`, whose roster is the backing PM project's members
 * (governed by PM and synced server-side), a `user_group` IS just a
 * `chat_sessions` row: its members are the session's `participants`, managed
 * directly through the user-side routes (`projectGroupStore.addMembers` /
 * `removeMember`). Any current member may invite or remove — the backend gates
 * on the participant relation and refuses a removal past the two-member floor.
 *
 * Names are resolved live (agents from the local registry, humans from the
 * account directory), never snapshotted, matching the `GroupChatPanel` header.
 */

import { useEffect, useMemo, useState } from "react";
import { LogOut, Plus, X, Search } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { CAPSULE_PANE_CN } from "../common/capsule";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { AgentAvatar } from "../common/AgentAvatar";
import { UserAvatar } from "../common/UserAvatar";
import { showToast } from "../common/ToastProvider";
import { useAuthStore } from "../../stores/authStore";
import { useAgentStore } from "../../stores/agentStore";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import { fetchDirectory } from "../../lib/auth-api";
import { getGatewayUrl } from "../../lib/config";
import type { DirectoryUser } from "../../lib/types";
import type { ChatSessionInfo } from "../../stores/chatSessionStore";

interface Props {
  group: ChatSessionInfo;
  width: number;
  onResizeStart?: (e: React.MouseEvent) => void;
}

interface Named {
  id: string;
  name: string;
  isAgent: boolean;
  avatarUrl: string | null;
  builtinAvatarId: string | null;
}

export function UserGroupMembersPanel({ group, width, onResizeStart }: Props) {
  const { t } = useTranslation();
  const me = useAuthStore((s) => s.account?.user_id ?? "");
  const token = useAuthStore((s) => s.accessToken);
  const agents = useAgentStore((s) => s.agents);
  const removeMember = useProjectGroupStore((s) => s.removeMember);
  const [directory, setDirectory] = useState<DirectoryUser[]>([]);
  const [inviteOpen, setInviteOpen] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);

  useEffect(() => {
    if (!token) return;
    void fetchDirectory(getGatewayUrl(), token)
      .then(setDirectory)
      .catch(() => {});
  }, [token]);

  const participants = group.participants ?? [];
  const canManage = participants.some((p) => p.id === me);

  const nameOf = useMemo(() => {
    const byId = new Map<string, DirectoryUser>();
    for (const u of directory) byId.set(u.user_id, u);
    return (id: string): Named => {
      const ag = agents[id]?.meta;
      if (ag) {
        return {
          id,
          name: ag.display_name || ag.name || id,
          isAgent: true,
          avatarUrl: ag.avatar ?? null,
          builtinAvatarId: ag.builtin_avatar ?? null,
        };
      }
      const u = byId.get(id);
      if (u) {
        return {
          id,
          name: u.display_name || u.username,
          isAgent: false,
          avatarUrl: u.avatar ?? null,
          builtinAvatarId: u.builtin_avatar ?? null,
        };
      }
      return { id, name: id.slice(0, 8), isAgent: false, avatarUrl: null, builtinAvatarId: null };
    };
  }, [agents, directory]);

  const doRemove = async (pid: string) => {
    setBusy(pid);
    try {
      await removeMember(group.session_id, pid);
    } catch (e) {
      showToast({ type: "error", message: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="relative ml-1 flex shrink-0 flex-col" style={{ width }}>
      {onResizeStart && (
        <div
          className="group absolute -left-1 top-0 bottom-0 z-10 w-1 cursor-col-resize"
          onMouseDown={onResizeStart}
        >
          <div className="absolute inset-y-0 left-0 w-1 rounded-full transition-colors group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60" />
        </div>
      )}

      <div className={cn(CAPSULE_PANE_CN, "flex-1 bg-right-panel")}>
        <div className="flex min-h-[var(--ui-list-header-h)] shrink-0 items-center border-b border-border-divider px-[var(--tab-px)] text-[length:var(--tab-font-size)] font-medium leading-[var(--tab-line-height)] text-text-tertiary">
          <span>
            {t("groupMembers.title")}（{participants.length}）
          </span>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto p-3">
          <div className="flex flex-wrap gap-x-2 gap-y-4">
            {participants.map((p) => {
              const who = nameOf(p.id);
              const isSelf = p.id === me;
              return (
                <div
                  key={p.id}
                  className="relative flex w-16 shrink-0 flex-col items-center gap-1"
                  title={who.name}
                >
                  {who.isAgent ? (
                    <AgentAvatar
                      agentId={p.id}
                      displayName={who.name}
                      avatarUrl={who.avatarUrl}
                      builtinAvatarId={who.builtinAvatarId}
                      size={44}
                      className="rounded-lg"
                    />
                  ) : (
                    <UserAvatar
                      displayName={who.name}
                      avatarUrl={who.avatarUrl}
                      builtinAvatarId={who.builtinAvatarId}
                      size={44}
                      className="rounded-lg"
                    />
                  )}
                  <span className="w-full truncate text-center text-10 text-text-secondary">
                    {who.name}
                    {isSelf ? " " + t("common.you") : ""}
                  </span>
                  {canManage && (
                    <button
                      type="button"
                      disabled={busy === p.id}
                      onClick={() => void doRemove(p.id)}
                      aria-label={t("groupMembers.removeAria", { name: who.name })}
                      className="absolute -right-1 -top-1 flex h-4 w-4 items-center justify-center rounded-full bg-zinc-500 text-white opacity-0 transition-opacity hover:bg-red-500 focus:opacity-100 group-hover:opacity-100 [div:hover>&]:opacity-100 disabled:opacity-40"
                    >
                      <X className="h-2.5 w-2.5" />
                    </button>
                  )}
                </div>
              );
            })}
            {canManage && (
              <button
                type="button"
                onClick={() => setInviteOpen(true)}
                aria-label={t("groupMembers.add")}
                className="flex w-16 shrink-0 flex-col items-center gap-1"
              >
                <span className="flex h-11 w-11 items-center justify-center rounded-lg border border-dashed border-border-outer text-text-tertiary transition-colors hover:border-[var(--color-accent)]/50 hover:text-[var(--color-accent)]">
                  <Plus size={18} />
                </span>
                <span className="w-full truncate text-center text-10 text-text-secondary">
                  {t("groupMembers.add")}
                </span>
              </button>
            )}
          </div>
        </div>

        {canManage && (
          <div className="shrink-0 border-t border-border-divider p-3">
            <button
              type="button"
              disabled={busy === me}
              onClick={() => void doRemove(me)}
              className="inline-flex items-center gap-1.5 text-xs text-red-600 hover:underline disabled:opacity-40 dark:text-red-400"
            >
              <LogOut className="h-3.5 w-3.5" />
              {t("groupMembers.leave")}
            </button>
          </div>
        )}
      </div>

      {inviteOpen && (
        <InviteDialog
          group={group}
          directory={directory}
          onClose={() => setInviteOpen(false)}
        />
      )}
    </div>
  );
}

/** Invite picker for a `user_group` — the same candidate grammar as
 *  `NewGroupDialog` (agents + directory humans), but every already-listed
 *  participant is filtered out, since you can only invite someone new. */
function InviteDialog({
  group,
  directory,
  onClose,
}: {
  group: ChatSessionInfo;
  directory: DirectoryUser[];
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const agents = useAgentStore((s) => s.agents);
  const addMembers = useProjectGroupStore((s) => s.addMembers);
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const members = new Set((group.participants ?? []).map((p) => p.id));
  const candidates = useMemo(() => {
    const rows: Named[] = [];
    for (const u of directory) {
      if (!members.has(u.user_id)) {
        rows.push({
          id: u.user_id,
          name: u.display_name || u.username,
          isAgent: false,
          avatarUrl: u.avatar ?? null,
          builtinAvatarId: u.builtin_avatar ?? null,
        });
      }
    }
    for (const a of Object.values(agents)) {
      if (!members.has(a.meta.instance_id)) {
        rows.push({
          id: a.meta.instance_id,
          name: a.meta.display_name || a.meta.name || a.meta.instance_id,
          isAgent: true,
          avatarUrl: a.meta.avatar ?? null,
          builtinAvatarId: a.meta.builtin_avatar ?? null,
        });
      }
    }
    const q = query.trim().toLowerCase();
    return rows
      .filter((c) => !q || c.name.toLowerCase().includes(q))
      .sort((a, b) => a.name.localeCompare(b.name));
    // `members` is rebuilt from `group.participants` each render; fold the
    // dependency on that so a change (after a doorbell refresh) recomputes.
  }, [directory, agents, query, group.participants]);

  const toggle = (id: string) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const confirm = async () => {
    if (selected.size === 0) return onClose();
    setBusy(true);
    try {
      await addMembers(group.session_id, [...selected]);
      onClose();
    } catch (e) {
      showToast({ type: "error", message: e instanceof Error ? e.message : String(e) });
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-modal-overlay" onClick={onClose} />
      <div
        className="relative z-10 flex max-h-[80vh] w-full max-w-md flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-xl"
        role="dialog"
        aria-modal="true"
        aria-label={t("groupMembers.inviteTitle")}
      >
        <header className="flex items-center justify-between border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <h3 className="text-sm font-semibold text-text">{t("groupMembers.inviteTitle")}</h3>
          <button
            type="button"
            onClick={onClose}
            className="rounded px-1.5 text-sm text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-700"
            aria-label={t("common.close")}
          >
            ×
          </button>
        </header>

        <div className="border-b border-border-divider px-5 py-3">
          <div className="flex items-center gap-2 rounded-md border border-border-outer bg-panel-block px-2">
            <Search className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
            <input
              autoFocus
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder={t("newGroup.searchPlaceholder")}
              className="h-8 w-full min-w-0 bg-transparent text-xs text-text outline-none placeholder:text-text-disabled"
            />
          </div>
        </div>

        <OverlayScrollbar fill="flex" className="px-2 py-2">
          {candidates.length === 0 ? (
            <p className="px-3 py-6 text-center text-xs italic text-text-tertiary">
              {t("groupMembers.noCandidates")}
            </p>
          ) : (
            <ul className="space-y-0.5">
              {candidates.map((c) => {
                const on = selected.has(c.id);
                return (
                  <li key={c.id}>
                    <button
                      type="button"
                      onClick={() => toggle(c.id)}
                      aria-pressed={on}
                      className={cn(
                        "flex w-full items-center gap-2 rounded-md px-3 py-1.5 text-left transition-colors",
                        on ? "bg-accent-strong/10" : "hover:bg-zinc-50 dark:hover:bg-zinc-800/60",
                      )}
                    >
                      {c.isAgent ? (
                        <AgentAvatar
                          agentId={c.id}
                          displayName={c.name}
                          avatarUrl={c.avatarUrl}
                          builtinAvatarId={c.builtinAvatarId}
                          size={24}
                        />
                      ) : (
                        <UserAvatar
                          displayName={c.name}
                          avatarUrl={c.avatarUrl}
                          builtinAvatarId={c.builtinAvatarId}
                          size={24}
                        />
                      )}
                      <span className="min-w-0 flex-1 truncate text-xs text-text">{c.name}</span>
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </OverlayScrollbar>

        <footer className="flex items-center gap-2 border-t border-border-divider px-5 py-3">
          <span className="mr-auto text-11 text-text-tertiary">
            {t("newGroup.selected", { count: selected.size })}
          </span>
          <button
            type="button"
            onClick={onClose}
            disabled={busy}
            className="rounded-md px-3 py-1.5 text-xs text-text-secondary hover:bg-zinc-100 disabled:opacity-40 dark:hover:bg-zinc-700"
          >
            {t("newGroup.cancel")}
          </button>
          <button
            type="button"
            onClick={() => void confirm()}
            disabled={busy || selected.size === 0}
            className="btn-accent rounded-md px-3 py-1.5 text-xs font-medium disabled:opacity-40"
          >
            {t("groupMembers.add")}
          </button>
        </footer>
      </div>
    </div>
  );
}
