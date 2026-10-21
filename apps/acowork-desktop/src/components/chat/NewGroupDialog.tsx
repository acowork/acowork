/**
 * NewGroupDialog — create a user-defined `user_group` (P1, the "multi-user /
 * multi-agent group" that the standard-group model is built around).
 *
 * A `user_group` is one `chat_sessions` row whose participants are a mix of
 * human accounts and Agent instances; the backend `POST /api/chat-sessions`
 * already accepts that kind, so this dialog only gathers a title + a member
 * set and hands it to `projectGroupStore.createGroup` (which adds the caller
 * and opens the fresh group). Once created it renders through the shared
 * `GroupChatPanel` — messages / history search / attachments all inherit, no
 * new storage.
 *
 * Candidates mirror `MemberManagerDialog`'s grammar (agents from the local
 * registry, humans from the account directory — which excludes the caller, so
 * self is never offered). Selection is a plain toggle; the create is one call.
 */

import { useEffect, useMemo, useState } from "react";
import { Loader2, Search } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { AgentAvatar } from "../common/AgentAvatar";
import { UserAvatar } from "../common/UserAvatar";
import { useAgentStore } from "../../stores/agentStore";
import { useAuthStore } from "../../stores/authStore";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import { fetchDirectory } from "../../lib/auth-api";
import { getGatewayUrl } from "../../lib/config";
import type { DirectoryUser } from "../../lib/types";

interface NewGroupDialogProps {
  onClose: () => void;
}

interface Candidate {
  id: string;
  name: string;
  isAgent: boolean;
  avatarUrl: string | null;
  builtinAvatarId: string | null;
}

export function NewGroupDialog({ onClose }: NewGroupDialogProps) {
  const { t } = useTranslation();
  const agents = useAgentStore((s) => s.agents);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);
  const me = useAuthStore((s) => s.account?.user_id ?? "");
  const token = useAuthStore((s) => s.accessToken);
  const createGroup = useProjectGroupStore((s) => s.createGroup);

  const [name, setName] = useState("");
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const [directory, setDirectory] = useState<DirectoryUser[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Load both candidate sources on mount — agents may already be cached, the
  // directory is a one-shot read (best-effort; local mode returns nothing).
  useEffect(() => {
    if (Object.keys(agents).length === 0) void fetchAgents();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  useEffect(() => {
    if (!token) return;
    void fetchDirectory(getGatewayUrl(), token)
      .then(setDirectory)
      .catch(() => {});
  }, [token]);

  // ESC to dismiss.
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const candidates = useMemo<Candidate[]>(() => {
    const rows: Candidate[] = [];
    // The directory excludes the caller, but guard anyway — self is added
    // server-side and must never appear as a togglable candidate.
    for (const u of directory) {
      if (u.user_id === me) continue;
      rows.push({
        id: u.user_id,
        name: u.display_name || u.username,
        isAgent: false,
        avatarUrl: u.avatar ?? null,
        builtinAvatarId: u.builtin_avatar ?? null,
      });
    }
    for (const a of Object.values(agents)) {
      rows.push({
        id: a.meta.instance_id,
        name: a.meta.display_name || a.meta.name || a.meta.instance_id,
        isAgent: true,
        avatarUrl: a.meta.avatar ?? null,
        builtinAvatarId: a.meta.builtin_avatar ?? null,
      });
    }
    const q = query.trim().toLowerCase();
    return rows
      .filter((c) => !q || c.name.toLowerCase().includes(q))
      .sort((a, b) => a.name.localeCompare(b.name));
  }, [directory, agents, me, query]);

  const toggle = (id: string) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const submit = async () => {
    const title = name.trim();
    if (!title) {
      setError(t("newGroup.nameLabel"));
      return;
    }
    // Self counts as one member, so the roster needs at least one selection to
    // reach the standard group's ≥2 floor (mirrors the backend create guard).
    if (selected.size === 0) {
      setError(t("groupMembers.minTwoNote"));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await createGroup(title, [...selected]);
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : t("newGroup.error"));
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
        aria-label={t("newGroup.title")}
      >
        <header className="flex items-center justify-between border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <h3 className="text-sm font-semibold text-text">{t("newGroup.title")}</h3>
          <button
            type="button"
            onClick={onClose}
            className="rounded px-1.5 text-sm text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-700"
            aria-label={t("common.close")}
          >
            ×
          </button>
        </header>

        <div className="flex flex-col gap-3 border-b border-border-divider px-5 py-3">
          <input
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder={t("newGroup.namePlaceholder")}
            aria-label={t("newGroup.nameLabel")}
            className="h-9 w-full rounded-md border border-border-outer bg-panel-block px-3 text-sm text-text outline-none placeholder:text-text-disabled focus:border-[var(--color-accent)]/50"
          />
          <div className="flex items-center gap-2 rounded-md border border-border-outer bg-panel-block px-2">
            <Search className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
            <input
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
              {t("newGroup.empty")}
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
                      <span className="min-w-0 flex-1">
                        <span className="block truncate text-xs text-text">{c.name}</span>
                        <span className="block truncate text-10 text-text-tertiary">{c.id}</span>
                      </span>
                      <span
                        className={cn(
                          "flex h-4 w-4 shrink-0 items-center justify-center rounded-full border text-10",
                          on
                            ? "border-[var(--color-accent)] bg-[var(--color-accent)] text-white"
                            : "border-border-outer text-transparent",
                        )}
                        aria-hidden
                      >
                        ✓
                      </span>
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </OverlayScrollbar>

        <footer className="flex items-center gap-2 border-t border-border-divider px-5 py-3">
          {error && <span className="mr-auto truncate text-11 text-red-500">{error}</span>}
          {!error && (
            <span className="mr-auto text-11 text-text-tertiary">
              {t("newGroup.selected", { count: selected.size })}
            </span>
          )}
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
            onClick={() => void submit()}
            disabled={busy || name.trim().length === 0}
            className="btn-accent inline-flex items-center gap-1.5 rounded-md px-3 py-1.5 text-xs font-medium disabled:opacity-40"
          >
            {busy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
            {t("newGroup.create")}
          </button>
        </footer>
      </div>
    </div>
  );
}
