/**
 * The action bar shared by the Hub card and the Local file card.
 *
 * PRD: docs/prd/zh/prd-agent-hub-install-dialog.md (D2 / D3 / REQ-005,
 * REQ-035–038).
 *
 * Two actions, and they are different intents — that is the whole point of
 * splitting them onto two buttons instead of re-labelling one:
 *
 *   - **安装** always means "get this package onto the target node". When the
 *     node holds no instance of it yet that is the idempotent `/ensure`, so a
 *     double click cannot create two instances; once an instance exists the
 *     same button lands one more (`/install`), because multi-instance is legal
 *     (ADR-073) and "already there, nothing happened" reads as broken.
 *   - **升级** only appears when the target node runs an OLDER version of the
 *     package, and it replaces those instances in place
 *     (`POST /api/agents/{instance_id}/upgrade` — package content swapped,
 *     `workspace/` preserved). `/ensure` cannot stand in for it: presence is
 *     answered by package id, so a newer package would be a silent no-op.
 *
 * The two never collapse into one button: an upgrade overwrites what is
 * running, an install adds alongside it, and the user picks.
 */
import { Loader2 } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";

/** Which of the two actions a card/row currently has in flight. */
export type InstallAction = "install" | "upgrade";

interface AgentInstallActionsProps {
  /** Instances on the target node older than the candidate package version. */
  outdatedCount: number;
  /**
   * `安装` resolves to the idempotent `/ensure` — true while the node holds
   * no instance yet, and for a package THIS VISIT installed that way (so the
   * button does not change meaning under the cursor, REQ-005).
   */
  installIdempotent: boolean;
  /** The action in flight on THIS card — everything else is disabled. */
  installing: InstallAction | null;
  disabled: boolean;
  disabledReason?: string;
  onInstall: () => void;
  onUpgrade: () => void;
}

export function AgentInstallActions({
  outdatedCount,
  installIdempotent,
  installing,
  disabled,
  disabledReason,
  onInstall,
  onUpgrade,
}: AgentInstallActionsProps) {
  const { t } = useTranslation();
  const blocked = disabled || installing !== null;
  const upgradeable = outdatedCount > 0;
  const primary =
    "btn-accent flex shrink-0 items-center gap-1.5 rounded px-2.5 py-1 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50";
  const secondary =
    "flex shrink-0 items-center gap-1.5 rounded border border-border-outer px-2.5 py-1 text-xs font-medium text-text-secondary hover:bg-[var(--color-accent)]/10 disabled:cursor-not-allowed disabled:opacity-50 dark:hover:bg-[var(--color-accent)]/15";

  return (
    <div className="flex shrink-0 items-center gap-1.5">
      {upgradeable && (
        <button
          type="button"
          onClick={onUpgrade}
          disabled={blocked}
          title={disabledReason}
          className={primary}
        >
          {installing === "upgrade" && <Loader2 className="h-3 w-3 animate-spin" />}
          {installing === "upgrade" ? t("agentHub.upgrading") : t("agentHub.upgrade")}
        </button>
      )}
      <button
        type="button"
        onClick={onInstall}
        disabled={blocked}
        title={disabledReason}
        className={upgradeable ? secondary : primary}
      >
        {installing === "install" && <Loader2 className="h-3 w-3 animate-spin" />}
        {installing === "install"
          ? t("agentHub.installing")
          : installIdempotent
            ? t("agentHub.install")
            : t("agentHub.installAnother")}
      </button>
    </div>
  );
}