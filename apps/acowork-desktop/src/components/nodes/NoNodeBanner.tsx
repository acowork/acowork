/**
 * "No usable node" empty state — shared by the Agent library dialog and the
 * first-run onboarding step.
 *
 * ADR-113: "there is no node to install onto" is a protocol state, not a
 * Gateway-mode condition. The two recovery paths are therefore identical in
 * every topology and are never gated on `gatewayMode`:
 *
 *   1. Start a node on THIS machine — an ordinary Desktop capability.
 *   2. Start a node on ANOTHER machine — see RemoteNodeEnrollPanel (also
 *      reachable from the Agent-list `+` menu at any time).
 */
import { useState } from "react";
import { Loader2 } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { RemoteNodeEnrollPanel } from "./RemoteNodeEnrollPanel";

export function NoNodeBanner({
  starting,
  onStart,
  detail = null,
}: {
  starting: boolean;
  onStart: () => void;
  /** Extra protocol-state explanation (e.g. nodes exist but are not ready
   *  or not manageable). Never a Gateway-mode statement. */
  detail?: string | null;
}) {
  const { t } = useTranslation();
  const [guideOpen, setGuideOpen] = useState(false);

  return (
    <div className="mb-3 rounded-md border border-border-outer bg-panel-block px-3 py-2.5">
      <div className="flex items-center gap-2">
        <div className="min-w-0 flex-1">
          <p className="text-xs font-medium text-text">{t("agentHub.noAvailableNode")}</p>
          <p className="mt-0.5 text-10 text-text-tertiary">
            {detail ?? t("agentHub.noNodeHint")}
          </p>
        </div>
        <button
          type="button"
          onClick={() => setGuideOpen((v) => !v)}
          aria-expanded={guideOpen}
          className="shrink-0 rounded px-2 py-1 text-xs text-text-secondary hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15"
        >
          {t("agentHub.remoteNodeGuide")}
        </button>
        <button
          type="button"
          onClick={onStart}
          disabled={starting}
          title={starting ? t("agentHub.startLocalNodeStarting") : undefined}
          className="btn-accent flex shrink-0 items-center gap-1.5 rounded px-2.5 py-1 text-xs font-medium disabled:opacity-50"
        >
          {starting && <Loader2 className="h-3 w-3 animate-spin" />}
          {starting ? t("agentHub.startLocalNodeStarting") : t("agentHub.startLocalNode")}
        </button>
      </div>

      {guideOpen && (
        <div className="mt-2 border-t border-border-divider pt-2">
          <RemoteNodeEnrollPanel />
        </div>
      )}
    </div>
  );
}
