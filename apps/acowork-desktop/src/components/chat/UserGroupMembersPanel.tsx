/**
 * UserGroupMembersPanel — the right-hand member surface for a user-created
 * `user_group` (P1/P2).
 *
 * Unlike a `project_group`, whose roster is the backing PM project's members
 * (governed by PM and synced server-side), a `user_group` IS just a
 * `chat_sessions` row: its members are the session's `participants`, managed
 * directly through the user-side routes (`projectGroupStore.addMembers` /
 * `removeMember`). Any current member may invite, but removing ANOTHER member
 * is owner-or-admin only (ADR-098 governance, revised) — the backend gates on
 * that relation and refuses a removal past the two-member floor. Anyone may
 * leave via the bottom button (self-removal is always allowed).
 *
 * Governance (ADR-098) mirrors the project group: the founder is the owner
 * (`group.owner`), a permanent member who cannot be removed and cannot leave
 * until ownership is transferred. Rename and transfer are owner-or-admin
 * gated — the affordances only show for that set, and the backend re-checks
 * every request (a stranger gets the hidden 404, a member 403).
 *
 * Names are resolved live (agents from the local registry, humans from the
 * account directory), never snapshotted, matching the `GroupChatPanel` header.
 */

import { useEffect, useMemo, useState } from "react";
import { Check, Crown, LogOut, Pencil, Plus, Search, X } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { CAPSULE_PANE_CN } from "../common/capsule";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { ListBox, ExpandableRow } from "../common/list";
import { AgentAvatar } from "../common/AgentAvatar";
import { UserAvatar } from "../common/UserAvatar";
import { showToast } from "../common/ToastProvider";
import { GroupChatPromptCard } from "./GroupChatPromptCard";
import { canGovern } from "../../views/pm/pmMembers";
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
  const account = useAuthStore((s) => s.account);
  const localMode = useAuthStore((s) => s.mode) === "local";
  const token = useAuthStore((s) => s.accessToken);
  const agents = useAgentStore((s) => s.agents);
  const removeMember = useProjectGroupStore((s) => s.removeMember);
  const renameGroup = useProjectGroupStore((s) => s.renameGroup);
  const transferOwner = useProjectGroupStore((s) => s.transferOwner);
  const [directory, setDirectory] = useState<DirectoryUser[]>([]);
  const [inviteOpen, setInviteOpen] = useState(false);
  const [renameOpen, setRenameOpen] = useState(false);
  const [membersOpen, setMembersOpen] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);

  useEffect(() => {
    if (!token) return;
    void fetchDirectory(getGatewayUrl(), token)
      .then(setDirectory)
      .catch(() => {});
  }, [token]);

  const participants = group.participants ?? [];
  const canManage = participants.some((p) => p.id === me);
  // ADR-098 governance: owner-or-admin, mirroring the backend's
  // `can_govern_group` in the session lock. A plain member gets neither the
  // rename / transfer / remove affordances nor the group prompt card.
  const owner = group.owner ?? "";
  const isOwner = !!owner && owner === me;
  const governs = canManage && canGovern(owner, account, localMode);

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
      // The account directory excludes the caller, so self would otherwise
      // fall through to the id fragment ("22063a06"). Resolve the current
      // user from the auth store — name and avatar are authoritative there.
      if (account && id === account.user_id) {
        return {
          id,
          name: account.display_name || account.username,
          isAgent: false,
          avatarUrl: account.avatar ?? null,
          builtinAvatarId: account.builtin_avatar ?? null,
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
  }, [agents, directory, account]);

  /** Map the store's thrown `"METHOD path -> STATUS "` to a localized toast;
   *  the backend's prose (English) stays a last-resort fallback. */
  const governError = (e: unknown): string => {
    const m = e instanceof Error ? e.message : String(e);
    if (m.includes("-> 403")) return t("groupMembers.governForbidden");
    if (m.includes("-> 422")) return t("groupMembers.transferFailed");
    return m;
  };

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

  const doTransfer = async (pid: string) => {
    setBusy(pid);
    try {
      await transferOwner(group.session_id, pid);
      showToast({ type: "success", message: t("groupMembers.transferSaved") });
    } catch (e) {
      showToast({ type: "error", message: governError(e) });
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
        {/* Panel header — a generic settings title, matching the agent
            RightPanel tab header. The group's own name is edited inside the
            members card now (the rename affordance moved off this line). */}
        <div className="flex min-h-[var(--ui-list-header-h)] shrink-0 items-center border-b border-border-divider px-[var(--tab-px)] text-[length:var(--tab-font-size)] font-medium leading-[var(--tab-line-height)] text-text-tertiary">
          <span className="min-w-0 flex-1 truncate">{t("groupSettings.title")}</span>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto p-3">
          {/* Members card — the unified `ListBox` + `ExpandableRow` grammar of
              the agent Settings tab. Rename is owner/admin-only and sits in the
              card header's trailing slot (backend re-checks under the session
              lock; a plain member sees no pencil). */}
          <ListBox dividers={false}>
            <ExpandableRow
              open={membersOpen}
              onToggle={() => setMembersOpen((v) => !v)}
              title={t("groupMembers.title")}
              ariaLabel={t("groupMembers.title")}
              meta={
                <span className="shrink-0 rounded bg-panel-inset px-1.5 py-0.5 text-10 text-text-tertiary">
                  {participants.length}
                </span>
              }
              trailing={
                governs ? (
                  <span className="flex shrink-0 items-center" onClick={(e) => e.stopPropagation()}>
                    <button
                      type="button"
                      onClick={() => setRenameOpen(true)}
                      aria-label={t("groupMembers.rename")}
                      title={t("groupMembers.renameTitle")}
                      data-testid="group-rename-button"
                      className="flex h-5 w-5 items-center justify-center rounded text-text-tertiary transition-colors hover:bg-nav-item-hover hover:text-zinc-600 dark:hover:text-zinc-300"
                    >
                      <Pencil className="h-3 w-3" />
                    </button>
                  </span>
                ) : undefined
              }
              bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-3"
            >
          <div className="flex flex-wrap gap-x-2 gap-y-4">
            {participants.map((p) => {
              const who = nameOf(p.id);
              const isSelf = p.id === me;
              const isOwnerTile = !!owner && p.id === owner;
              // A human participant (identity is authoritative on the wire;
              // the local registry is the fallback). Only a human can own a
              // group — the backend refuses an agent target.
              const isHuman = p.identity ? p.identity !== "agent" : !who.isAgent;
              // The owner tile is permanent: never removable, never a target.
              // Removing ANOTHER member is owner-or-admin only (the backend
              // re-checks); a plain member sees no ×, and leaves via the
              // bottom button instead (self-removal always allowed).
              const canRemove = governs && !isOwnerTile;
              const canSetOwner = governs && !isOwnerTile && isHuman;
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
                  {isOwnerTile ? (
                    // The owner is a permanent member — no remove affordance
                    // (the backend refuses); transfer first to demote.
                    <span className="flex items-center gap-0.5 text-10 font-medium text-[var(--color-accent)]">
                      <Crown className="h-2.5 w-2.5" />
                      {t("groupMembers.owner")}
                    </span>
                  ) : (
                    <span className="h-4" />
                  )}
                  {canRemove && (
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
                  {/* Transfer target: any human member can own the group (the
                      backend refuses agents and strangers). Opposite corner
                      from the remove × so both can coexist on one tile. */}
                  {canSetOwner && (
                    <button
                      type="button"
                      disabled={busy === p.id}
                      onClick={() => void doTransfer(p.id)}
                      aria-label={t("groupMembers.makeOwnerAria", { name: who.name })}
                      title={t("groupMembers.makeOwner")}
                      className="absolute -left-1 -top-1 flex h-4 w-4 items-center justify-center rounded-full bg-[var(--color-accent)] text-white opacity-0 transition-opacity focus:opacity-100 group-hover:opacity-100 [div:hover>&]:opacity-100 disabled:opacity-40"
                    >
                      <Crown className="h-2.5 w-2.5" />
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
            </ExpandableRow>
          </ListBox>
          {/* Group chat prompt — its own card (same `ListBox`/`ExpandableRow`
              grammar as the members card). Owner-or-admin only (ADR-098): a
              plain member gets no card at all — neither a read of the prompt
              nor an edit the backend would refuse. */}
          {governs && <GroupChatPromptCard sessionId={group.session_id} />}
        </div>

        {canManage && (
          <div className="shrink-0 border-t border-border-divider p-3">
            {/* Global accent button (`.btn-accent`), same as the clone/publish
                agent dialog primary action. The owner cannot leave their own
                group — the backend refuses until ownership moves, so the button
                is disabled with the reason as its tooltip. */}
            <button
              type="button"
              disabled={busy === me || isOwner}
              title={isOwner ? t("groupMembers.mustTransferBeforeLeave") : undefined}
              onClick={() => void doRemove(me)}
              className="btn-accent inline-flex items-center gap-1.5 rounded px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
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

      {renameOpen && (
        <RenameDialog
          group={group}
          onClose={() => setRenameOpen(false)}
          onSubmit={async (title) => {
            await renameGroup(group.session_id, title);
            showToast({ type: "success", message: t("groupMembers.renameSaved") });
            setRenameOpen(false);
          }}
        />
      )}
    </div>
  );
}

/** Rename dialog for the group's title — owner/admin only (the panel hides
 *  the affordance, the backend enforces the gate). Plain form, Enter submits. */
function RenameDialog({
  group,
  onClose,
  onSubmit,
}: {
  group: ChatSessionInfo;
  onClose: () => void;
  onSubmit: (title: string) => Promise<void>;
}) {
  const { t } = useTranslation();
  const [title, setTitle] = useState(group.title ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const confirm = async () => {
    const trimmed = title.trim();
    if (!trimmed || busy) return;
    setBusy(true);
    setError(null);
    try {
      await onSubmit(trimmed);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-modal-overlay" onClick={onClose} />
      <div
        className="relative z-10 w-full max-w-sm rounded-md border border-border-outer bg-modal-surface shadow-xl"
        role="dialog"
        aria-modal="true"
        aria-label={t("groupMembers.renameTitle")}
      >
        <header className="flex items-center justify-between border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <h3 className="text-sm font-semibold text-text">{t("groupMembers.renameTitle")}</h3>
          <button
            type="button"
            onClick={onClose}
            className="rounded px-1.5 text-sm text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-700"
            aria-label={t("common.close")}
          >
            ×
          </button>
        </header>
        <div className="px-5 py-4">
          <input
            autoFocus
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void confirm();
            }}
            placeholder={t("groupMembers.renamePlaceholder")}
            className="h-8 w-full rounded-md border border-border-outer bg-panel-block px-2 text-xs text-text outline-none placeholder:text-text-disabled focus:border-[var(--color-accent)]/50"
          />
          {error && <p className="mt-2 text-11 text-red-600 dark:text-red-400">{error}</p>}
        </div>
        <footer className="flex items-center justify-end gap-2 border-t border-border-divider px-5 py-3">
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
            disabled={busy || !title.trim()}
            className="btn-accent inline-flex items-center gap-1.5 rounded-md px-3 py-1.5 text-xs font-medium disabled:opacity-40"
          >
            <Check className="h-3.5 w-3.5" />
            {t("groupMembers.rename")}
          </button>
        </footer>
      </div>
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
