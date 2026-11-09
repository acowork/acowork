/**
 * Local tab — the machine's own agent files.
 *
 * PRD: docs/prd/zh/prd-agent-hub-install-dialog.md §14 (D9–D14,
 * REQ-021–032). The tab renders the library directory (`list_local_agent_packages`
 * returns all rows in one call, F13) and ends with a fixed
 * "+ add an agent file" row that keeps the v0.1 pick-a-file path alive
 * (REQ-025 / A5: it installs the picked file directly and does **not** copy it
 * into the library).
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
 */
import { MoreHorizontal, Plus, X } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { formatSizeBytes, type LocalAgentList, type LocalAgentRow } from "../../lib/agentHub";
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
    <div className="flex flex-col gap-2">
      <p className="text-xs text-text-tertiary">{t("agentHub.localDesc")}</p>

      {loadError && <Notice tone="error" message={loadError} />}

      {list === null && !loadError ? (
        <div className="flex flex-col gap-2" aria-busy="true">
          {Array.from({ length: 3 }, (_, i) => (
            <div key={i} className="h-16 animate-pulse rounded-md border border-border-outer bg-panel-block" />
          ))}
        </div>
      ) : rows.length === 0 && !loadError ? (
        <div className="flex flex-col items-center gap-1.5 rounded-md border border-border-outer bg-panel-block px-4 py-8 text-center">
          <p className="text-xs font-medium text-text-secondary">{t("agentHub.localEmpty")}</p>
          {emptyReason && <p className="max-w-md text-10 text-text-tertiary">{emptyReason}</p>}
        </div>
      ) : (
        rows.map((row) => (
          <LocalAgentCard
            key={row.path}
            row={row}
            busy={busyPath === row.path}
            disabled={disabled}
            disabledReason={disabledReason}
            onInstall={() => onInstall(row)}
            onMore={(e) => onInstallAnother(row, e)}
          />
        ))
      )}

      {pending}

      {/* REQ-025: the trailing row is always present — it is the only way back
          to "I have a file on disk", including when the library is empty. */}
      <button
        type="button"
        onClick={onAddFile}
        disabled={disabled}
        title={disabledReason}
        className="flex items-center justify-center gap-1.5 rounded-md border border-dashed border-zinc-300 px-3 py-2.5 text-xs text-text-secondary transition-colors hover:border-[var(--color-accent)]/60 hover:text-text disabled:cursor-not-allowed disabled:opacity-50 dark:border-zinc-600"
      >
        <Plus className="h-3.5 w-3.5" />
        {t("agentHub.localAddFile")}
      </button>
    </div>
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

  return (
    <article
      className={cn(
        "flex items-start gap-3 rounded-md border bg-panel-block p-3",
        broken ? "border-border-outer opacity-80" : "border-border-outer",
      )}
    >
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-2">
          <h4 className="truncate text-sm font-semibold text-text">{displayName}</h4>
          {row.version && <span className="shrink-0 text-10 text-text-tertiary">v{row.version}</span>}
        </div>

        {/* REQ-024: the file name is shown because it is what the user sees in
            the folder; the package id is shown because it is what the Gateway
            keys on. Both are labels — neither is parsed to derive the other.
            Skipped when the manifest has no name (a broken file): the heading
            above already *is* the file name there, and repeating it reads like
            two different things. */}
        {row.name && row.name !== row.file_name && (
          <p className="truncate text-10 text-text-tertiary">{row.file_name}</p>
        )}
        {row.agent_id && <p className="truncate text-10 text-text-tertiary">{row.agent_id}</p>}

        {row.description && (
          <p className="mt-1 line-clamp-2 text-xs text-text-tertiary">{row.description}</p>
        )}

        {broken ? (
          <div className="mt-1.5 flex items-start gap-1.5 rounded bg-red-50 px-2 py-1 text-10 text-red-700 dark:bg-red-900/20 dark:text-red-300">
            <X className="mt-0.5 h-3 w-3 shrink-0" />
            <span className="break-all">{t("agentHub.localBrokenFile", { error: row.error ?? "" })}</span>
          </div>
        ) : (
          <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-10 text-text-tertiary">
            {row.size_bytes !== null && <span>{formatSizeBytes(row.size_bytes)}</span>}
            {permissionCount > 0 && (
              <span>{t("agentHub.localPermissionCount", { count: permissionCount })}</span>
            )}
          </div>
        )}
      </div>

      <div className="flex shrink-0 items-center gap-1">
        <button
          type="button"
          onClick={onInstall}
          disabled={blocked || busy}
          title={blockedReason ?? (busy ? t("agentHub.installing") : undefined)}
          className="btn-accent rounded px-2.5 py-1 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
        >
          {busy ? t("agentHub.installing") : t("agentHub.install")}
        </button>
        <button
          type="button"
          onClick={onMore}
          disabled={blocked}
          title={blockedReason}
          aria-label={t("agentHub.moreActions")}
          aria-haspopup="menu"
          className="rounded p-1 text-text-tertiary hover:bg-zinc-100 disabled:opacity-40 dark:hover:bg-zinc-700"
        >
          <MoreHorizontal className="h-4 w-4" />
        </button>
      </div>
    </article>
  );
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
