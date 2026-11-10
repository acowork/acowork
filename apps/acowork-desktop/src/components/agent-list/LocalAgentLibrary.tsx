/**
 * Local tab — the machine's own agent files.
 *
 * PRD: docs/prd/zh/prd-agent-hub-install-dialog.md §14 (D9–D14,
 * REQ-021–032). The tab renders the library directory (`list_local_agent_packages`
 * returns all rows in one call, F13) as a two-column grid whose **first cell** is
 * the "+ add an agent file" tile — the v0.1 pick-a-file path, which installs the
 * picked file directly and does **not** copy it into the library (REQ-025 / A5).
 * First, not last: the library grows with use, and an entry pinned to the bottom
 * is out of view exactly when there are enough files to need it.
 *
 * Deliberately absent, and not to be added here:
 *   - any signature / channel wording (D13 / REQ-031 / A10);
 *   - an "installed" badge (D3 / REQ-026): existence is the Gateway's answer,
 *     the client does not compute state;
 *   - de-duplication by `agent_id` (REQ-030): one file is one row, so two
 *     versions of a package stay visible side by side.
 *
 * The row primary button is `ensure` (idempotent); the row menu's "install
 * another instance" is `/install` (multi-instance is legal, ADR-073).
 *
 * The row is drawn with the Hub card's skeleton — avatar block, name + role,
 * two-line description, chip row, divider, action bar — because the two tabs are
 * one list with two sources, not two designs. Every field it needs is read out
 * of the package itself (`manifest.toml` + the archive's `assets/` and
 * `skills/`), so the Local card is not the poorer one: a shipped `.agent`
 * carries the same things its published Hub entry does (see `read_package`).
 */
import { AlertTriangle, Loader2, MoreHorizontal, Plus, X } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import {
  formatSizeBytes,
  localChipsOf,
  type LocalAgentAvatar,
  type LocalAgentList,
  type LocalAgentRow,
} from "../../lib/agentHub";
import { InlinedAgentAvatar } from "../common/AgentAvatar";
import { cn } from "../../lib/utils";

interface LocalAgentLibraryProps {
  /** `null` = the first enumeration has not answered yet. */
  list: LocalAgentList | null;
  /** The enumeration itself failed (IPC error, not a per-file error). */
  loadError: string | null;
  /** Installs are serialized: everything is disabled while one is in flight. */
  disabled: boolean;
  disabledReason?: string;
  /** `path` of the row whose install is in flight — only it renders busy. */
  busyPath: string | null;
  onInstall: (row: LocalAgentRow) => void;
  onInstallAnother: (row: LocalAgentRow, e: React.MouseEvent) => void;
  onAddFile: () => void;
  /**
   * Rendered between the rows and the add entry: the file the user just picked
   * through that entry, or dropped (REQ-025). The add entry only picks; the
   * card installs from its own button, because every install commits from the
   * surface it belongs to and the dialog footer is the dismiss control.
   */
  pending?: React.ReactNode;
}

export function LocalAgentLibrary({
  list,
  loadError,
  disabled,
  disabledReason,
  busyPath,
  onInstall,
  onInstallAnother,
  onAddFile,
  pending,
}: LocalAgentLibraryProps) {
  const { t } = useTranslation();
  const rows = list?.rows ?? [];
  // REQ-028: an empty library needs a reason in the IO's own words, so the
  // seeding failure and "you deleted everything" do not look the same.
  const emptyReason = list?.warning ?? null;

  return (
    <div className="flex flex-col gap-3">
      <p className="text-xs text-text-tertiary">{t("agentHub.localDesc")}</p>

      {loadError && <Notice tone="error" message={loadError} />}

      {list === null && !loadError ? (
        <div className="grid grid-cols-1 gap-3 lg:grid-cols-2" aria-busy="true">
          {Array.from({ length: 4 }, (_, i) => (
            <div key={i} className="h-32 animate-pulse rounded-md border border-border-outer bg-panel-block" />
          ))}
        </div>
      ) : rows.length === 0 && !loadError ? (
        <div className="flex flex-col items-center gap-1.5 rounded-md border border-border-outer bg-panel-block px-4 py-8 text-center">
          <p className="text-xs font-medium text-text-secondary">{t("agentHub.localEmpty")}</p>
          {emptyReason && <p className="max-w-md text-10 text-text-tertiary">{emptyReason}</p>}
        </div>
      ) : null}

      {/* Same container as the Hub tab (`grid-cols-1 lg:grid-cols-2`): one agent
          per cell, two per row once the dialog is wide enough. The two tabs show
          the same kind of card, so they must pack the same way. */}
      <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
        {/* REQ-025: the add entry is the FIRST cell, not a trailing row. The
            library grows with use, so an entry pinned to the bottom slides out of
            view exactly when a user has enough files to need it; as a tile it
            keeps a fixed, findable position. It is also the only cell that is
            always present — an empty library still shows it. */}
        <AddAgentCard onClick={onAddFile} disabled={disabled} disabledReason={disabledReason} />
        {pending}
        {rows.map((row) => (
          <LocalAgentCard
            key={row.path}
            row={row}
            busy={busyPath === row.path}
            disabled={disabled}
            disabledReason={disabledReason}
            onInstall={() => onInstall(row)}
            onMore={(e) => onInstallAnother(row, e)}
          />
        ))}
      </div>
    </div>
  );
}

/**
 * The "+ add an agent file" tile.
 *
 * A card, not a row, because it lives in the grid: same cell as an agent, so it
 * scrolls and packs with the list instead of being pinned to its end. Dashed
 * border is the only visual difference — it holds no package, so it has nothing
 * to describe.
 */
function AddAgentCard({
  onClick,
  disabled,
  disabledReason,
}: {
  onClick: () => void;
  disabled: boolean;
  disabledReason?: string;
}) {
  const { t } = useTranslation();
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      title={disabledReason}
      className="flex h-full min-h-[7rem] flex-col items-center justify-center gap-1.5 rounded-md border border-dashed border-zinc-300 bg-panel-block p-3 text-xs text-text-secondary transition-colors hover:border-[var(--color-accent)]/60 hover:text-text disabled:cursor-not-allowed disabled:opacity-50 dark:border-zinc-600"
    >
      <Plus className="h-4 w-4" />
      {t("agentHub.localAddFile")}
    </button>
  );
}

function LocalAgentCard({
  row,
  busy,
  disabled,
  disabledReason,
  onInstall,
  onMore,
}: {
  row: LocalAgentRow;
  busy: boolean;
  disabled: boolean;
  disabledReason?: string;
  onInstall: () => void;
  onMore: (e: React.MouseEvent) => void;
}) {
  const { t } = useTranslation();
  const broken = row.error !== null;
  const blocked = disabled || broken;
  const blockedReason = disabledReason ?? (broken ? row.error ?? undefined : undefined);
  const displayName = row.name ?? row.file_name;
  const permissionCount = row.permissions.length;
  const chips = localChipsOf(row);

  return (
    // Same skeleton as the Hub card, on purpose: avatar block, name + role,
    // two-line description, chip row, then a divider and the action bar. The
    // Local tab is not a lesser surface — it lists the packages that ship with
    // the app — so the two must read as one list in two sources, not two
    // designs. The only structural addition is the identifiers line (REQ-024),
    // which has no Hub counterpart because a catalogue entry never shows a file
    // name.
    <article className="flex flex-col gap-2 rounded-md border border-border-outer bg-panel-block p-3">
      <div className="flex items-start gap-3">
        <LocalAvatar
          avatar={row.avatar}
          agentId={row.agent_id}
          initial={avatarInitialOf(displayName)}
          name={displayName}
        />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <h4 className="truncate text-sm font-semibold text-text">{displayName}</h4>
            {row.role && <span className="shrink-0 truncate text-xs text-text-tertiary">{row.role}</span>}
            {row.version && <span className="shrink-0 text-10 text-text-tertiary">v{row.version}</span>}
          </div>
          {row.description && (
            <p className="mt-0.5 line-clamp-2 text-xs text-text-tertiary">{row.description}</p>
          )}
        </div>
        <button
          type="button"
          onClick={onMore}
          disabled={blocked}
          title={blockedReason}
          aria-label={t("agentHub.moreActions")}
          aria-haspopup="menu"
          className="shrink-0 rounded p-1 text-text-tertiary hover:bg-zinc-100 disabled:opacity-40 dark:hover:bg-zinc-700"
        >
          <MoreHorizontal className="h-4 w-4" />
        </button>
      </div>

      {/* REQ-024: the file name is what the user sees in the folder and the
          package id is what the Gateway keys on. Both stay visible — neither is
          parsed to derive the other — but they belong under the identity, not
          competing with it. Skipped when the manifest is unreadable: the heading
          above is already the file name there, and printing it twice reads like
          two different things. */}
      {row.name && (
        <p className="truncate text-10 text-text-tertiary" title={`${row.file_name}\n${row.agent_id ?? ""}`}>
          {row.file_name}
          {row.agent_id && <span className="text-text-quaternary"> · </span>}
          {row.agent_id && <span className="text-text-quaternary">{row.agent_id}</span>}
        </p>
      )}

      {chips.length > 0 && (
        <div className="flex flex-wrap gap-1">
          {chips.map((chip) => (
            <span key={chip} className="rounded bg-zinc-100 px-1.5 py-0.5 text-10 text-text-secondary dark:bg-zinc-700">
              {chip}
            </span>
          ))}
        </div>
      )}

      {broken && (
        <div className="flex items-start gap-1.5 rounded-md bg-red-50 px-2 py-1 text-xs text-red-700 dark:bg-red-900/20 dark:text-red-300">
          <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span className="break-all">{t("agentHub.localBrokenFile", { error: row.error ?? "" })}</span>
        </div>
      )}

      <div className="mt-auto flex items-center justify-between gap-3 border-t border-border-outer pt-2">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-10 text-text-tertiary">
          {row.size_bytes !== null && <span>{formatSizeBytes(row.size_bytes)}</span>}
          {permissionCount > 0 && (
            <span>{t("agentHub.localPermissionCount", { count: permissionCount })}</span>
          )}
        </div>
        <button
          type="button"
          onClick={onInstall}
          disabled={blocked}
          title={blockedReason}
          className="btn-accent flex shrink-0 items-center gap-1.5 rounded px-2.5 py-1 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
        >
          {busy ? <Loader2 className="h-3 w-3 animate-spin" /> : null}
          {busy ? t("agentHub.installing") : t("agentHub.install")}
        </button>
      </div>
    </article>
  );
}

/**
 * The avatar block: whatever the package declared, drawn through the same
 * ADR-017 chain the Hub card uses — the image, then the builtin icon id, then
 * the deterministic icon keyed by `agent_id`.
 *
 * Nothing is fetched: `avatar` was resolved out of the archive by Rust
 * (`list_local_agent_packages`), so there is no bearer token, no cache and no
 * failure window beyond a corrupt image.
 *
 * The letter block is the one branch this file owns, because it is the one the
 * shared chain cannot answer: an unreadable file has no `agent_id`, so there is
 * no identity to key an icon by, and guessing one would put a random face on a
 * package nobody can identify.
 */
function LocalAvatar({
  avatar,
  agentId,
  initial,
  name,
}: {
  avatar: LocalAgentAvatar | null;
  agentId: string | null;
  initial: string;
  name: string;
}) {
  if (!agentId) {
    return (
      <div
        aria-label={name}
        className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-zinc-100 text-sm font-semibold text-text-secondary dark:bg-zinc-700"
      >
        {initial}
      </div>
    );
  }
  return (
    <InlinedAgentAvatar
      agentId={agentId}
      dataUrl={avatar?.kind === "packaged" ? avatar.data_url : null}
      builtinAvatarId={avatar?.kind === "builtin" ? avatar.icon_id : null}
      size={36}
      className="shrink-0"
    />
  );
}

function avatarInitialOf(name: string): string {
  return name.trim().charAt(0).toUpperCase() || "?";
}

function Notice({ tone, message }: { tone: "error"; message: string }) {
  return (
    <div
      className={cn(
        "flex items-start gap-2 rounded-md p-2 text-xs",
        tone === "error" && "bg-red-50 text-red-700 dark:bg-red-900/20 dark:text-red-300",
      )}
    >
      <X className="mt-0.5 h-3.5 w-3.5 shrink-0" />
      <span className="break-all">{message}</span>
    </div>
  );
}
