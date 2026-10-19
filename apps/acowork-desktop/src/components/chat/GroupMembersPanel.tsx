/**
 * GroupMembersPanel — the right-hand surface for a project group chat,
 * mirroring the agent session's `RightPanel` shell (capsule + left resize
 * handle, toggled from a 40px nav rail on its right). The layout follows
 * the WeChat group-info screen: a member grid with a search filter on top
 * and a trailing "+" tile that opens the member manager.
 *
 * Members are the backing PM project's members — the group's chat-session
 * participant list is synced from them server-side (acowork-pm
 * `group_participants` → `sync_group`), so listing `project.members` here
 * is the same truth the chat fans out to. Adding / removing reuses the PM
 * view's `MemberManagerDialog` (agent + user candidates, remove) so the two
 * surfaces can never drift on who counts as a member.
 */

import { useEffect, useMemo, useState } from "react";
import { Plus, Search, X } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { CAPSULE_PANE_CN } from "../common/capsule";
import { MemberAvatar, useResolvedMembers, type ResolvedMember } from "../../views/pm/pmMembers";
import { MemberManagerDialog } from "../../views/pm/MemberManagerDialog";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import { usePmProjectStore } from "../../stores/pm/projectStore";

interface GroupMembersPanelProps {
  width: number;
  onResizeStart?: (e: React.MouseEvent) => void;
  /** Collapse the panel (the rail toggle owns the same action). */
  onClose: () => void;
}

export function GroupMembersPanel({ width, onResizeStart, onClose }: GroupMembersPanelProps) {
  const { t } = useTranslation();
  const group = useProjectGroupStore(
    (s) => s.groups.find((g) => g.session_id === s.activeGroupId) ?? null,
  );
  const projects = usePmProjectStore((s) => s.projects);
  const loadProjects = usePmProjectStore((s) => s.loadProjects);
  const [query, setQuery] = useState("");
  const [managerOpen, setManagerOpen] = useState(false);

  const projectId = group?.project_id;
  // Re-derived from the store subscription, so an add / remove inside the
  // dialog (which updates `projects`) flows straight back into the grid.
  const project = useMemo(
    () => (projectId ? projects.find((p) => p.id === projectId) ?? null : null),
    [projects, projectId],
  );

  // The chat view doesn't load PM projects on its own — pull them in so the
  // member list can be resolved by id. `loadProjects` caches for 30s and is a
  // no-op while a fetch is in flight, so remounts don't thrash.
  useEffect(() => {
    if (projectId && !project) void loadProjects({ silent: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projectId]);

  const members = useResolvedMembers(project?.members);
  const filtered = useMemo(() => filterMembers(members, query), [members, query]);

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
        {/* Header — same line-box height contract as RightPanel's tab title
            so the hairline lines up with the chat pane's header divider. */}
        <div className="flex min-h-[var(--ui-list-header-h)] shrink-0 items-center justify-between border-b border-border-divider px-[var(--tab-px)] text-[length:var(--tab-font-size)] font-medium leading-[var(--tab-line-height)] text-text-tertiary">
          <span>
            {t("groupMembers.title")}
            {project ? `（${members.length}）` : ""}
          </span>
          <button
            type="button"
            onClick={onClose}
            aria-label={t("common.close")}
            className="rounded p-1 text-text-tertiary transition-colors hover:bg-zinc-200 hover:text-zinc-700 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
          >
            <X size={14} />
          </button>
        </div>

        {/* Search filter */}
        <div className="p-3 pb-0">
          <div className="flex items-center gap-2 rounded-md border border-border-outer bg-panel-block px-2">
            <Search className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
            <input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder={t("groupMembers.searchPlaceholder")}
              className="h-8 w-full min-w-0 bg-transparent text-xs text-text outline-none placeholder:text-text-disabled"
            />
          </div>
        </div>

        {/* Member grid — avatar over name, WeChat-style. The "+" tile trails
            the members (only once the project resolves, since adding goes
            through PM). */}
        <div className="min-h-0 flex-1 overflow-y-auto p-3">
          {!project ? (
            <div className="py-6 text-center text-xs text-text-tertiary">
              {t("groupMembers.loading")}
            </div>
          ) : (
            /* Fixed-width tiles that wrap: the avatar / name cell never
               stretches or shrinks with the panel width — instead the
               number of tiles per row adapts to the available space. A
               `grid-cols-N` would pin the column count and stretch each
               track (gaps widen when the panel grows, avatars squash when
               it shrinks), which is the wrong behaviour here. */
            <div className="flex flex-wrap gap-x-2 gap-y-4">
              {filtered.map((m) => (
                <MemberTile key={m.instance_id} member={m} />
              ))}
              <button
                type="button"
                onClick={() => setManagerOpen(true)}
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
            </div>
          )}
        </div>
      </div>

      {managerOpen && project && (
        <MemberManagerDialog project={project} onClose={() => setManagerOpen(false)} />
      )}
    </div>
  );
}

function MemberTile({ member }: { member: ResolvedMember }) {
  const name = member.displayName ?? member.instance_id;
  return (
    <div className="flex w-16 shrink-0 flex-col items-center gap-1" title={name}>
      <MemberAvatar member={member} size={44} className="rounded-lg" />
      <span className="w-full truncate text-center text-10 text-text-secondary">{name}</span>
    </div>
  );
}

/** Case-insensitive member filter for the search box. Matches the resolved
 *  display name, falling back to the raw id for unresolved members (an
 *  uninstalled agent / removed account still stays findable by its id).
 *  Exported as a pure function so the search behaviour is unit-testable
 *  without spinning up the panel's store graph. */
export function filterMembers(
  members: ReadonlyArray<ResolvedMember>,
  query: string,
): ResolvedMember[] {
  const q = query.trim().toLowerCase();
  if (!q) return [...members];
  return members.filter((m) =>
    (m.displayName ?? m.instance_id).toLowerCase().includes(q),
  );
}
