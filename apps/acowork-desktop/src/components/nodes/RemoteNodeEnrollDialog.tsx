/**
 * Modal wrapper around RemoteNodeEnrollPanel — opened from the Agent-list
 * `+` menu ("Start a node elsewhere"), always available even when a local
 * node already exists.
 *
 * Three-zone dialog chrome (header / body / footer), matching the other
 * app-wide dialogs: the footer carries the single Close action.
 */
import { useTranslation } from "../../i18n/useTranslation";
import { RemoteNodeEnrollPanel } from "./RemoteNodeEnrollPanel";

export function RemoteNodeEnrollDialog({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center p-4">
      <div className="absolute inset-0 bg-modal-overlay" onClick={onClose} />
      <div
        role="dialog"
        aria-modal="true"
        aria-label={t("agentHub.remoteNodeGuide")}
        className="relative z-10 w-full max-w-sm overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-xl"
      >
        <header className="flex items-center justify-between border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <h3 className="text-sm font-semibold text-text">{t("agentHub.remoteNodeGuide")}</h3>
          <button
            type="button"
            onClick={onClose}
            className="rounded px-1.5 text-sm leading-none text-text-tertiary hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15"
            aria-label={t("common.close")}
          >
            ×
          </button>
        </header>
        <div className="px-5 py-5">
          {/* Remount on each open so a fresh token is minted. */}
          <RemoteNodeEnrollPanel />
        </div>
        <footer className="flex items-center justify-end border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15"
          >
            {t("common.close")}
          </button>
        </footer>
      </div>
    </div>
  );
}
