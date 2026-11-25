import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AgentDetail } from "../../lib/types";
import { cn } from "../../lib/utils";
import { getGatewayUrl } from "../../lib/config";
import { useTranslation } from "../../i18n/useTranslation";
import { ErrorBox } from "../common/ErrorBox";
import { useEscapeClose } from "../../hooks/useEscapeClose";

interface AgentDetailDialogProps {
  open: boolean;
  agentId: string | null;
  onClose: () => void;
}

interface AgentModelInfo {
  provider: string;
  model: string;
  available_models: string[];
}

export function AgentDetailDialog({ open, agentId, onClose }: AgentDetailDialogProps) {
  const { t } = useTranslation();
  const [detail, setDetail] = useState<AgentDetail | null>(null);
  const [modelInfo, setModelInfo] = useState<AgentModelInfo | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const closeRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open || !agentId) return;

    setLoading(true);
    setError(null);
    invoke<AgentDetail>("get_agent_detail", { agentId })
      .then((d) => setDetail(d))
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));

    // Fetch model info from Gateway API
    fetch(`${getGatewayUrl()}/api/agents/${agentId}/model`)
      .then((resp) => resp.ok ? resp.json() as Promise<AgentModelInfo> : null)
      .then((data) => setModelInfo(data))
      .catch(() => setModelInfo(null));
  }, [open, agentId]);

  // Focus close on open — keyed on `open` alone. `AgentList` passes an
  // inline `onClose`, so listing it here re-ran `focus()` on every parent
  // re-render and stole focus from whatever the user was using.
  useEffect(() => {
    if (!open) return;
    closeRef.current?.focus();
  }, [open]);

  // Escape to close
  useEscapeClose(open, onClose);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      {/* Backdrop */}
      <div className="absolute inset-0 bg-modal-overlay" onClick={onClose} />

      {/* Dialog */}
      <div className="relative z-10 w-full max-w-md rounded-md border border-border-outer bg-modal-surface shadow-xl">
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <h3 className="text-sm font-semibold">Bot Details</h3>
          <button
            ref={closeRef}
            onClick={onClose}
            className="text-text-tertiary hover:text-text-tertiary"
            aria-label={t("agentDetailDialog.ariaLabelClose")}
          >
            ✕
          </button>
        </div>

        {/* Body */}
        <div className="space-y-3 px-5 py-4 text-xs">
          {loading && (
            <div className="flex items-center justify-center py-8">
              <div className="h-5 w-5 animate-spin rounded-full border-2 border-border-strong border-t-zinc-600 dark:border-t-zinc-300" />
            </div>
          )}

          {error && (
            <div className="text-sm">
              <ErrorBox message={`Failed to load bot details: ${error}`} onClose={() => setError(null)} />
            </div>
          )}

          {detail && !loading && (
            <div className="space-y-3 text-xs">
            <DetailRow label={t("agentDetailDialog.labelName")} value={detail.name} />
            <DetailRow label={t("agentDetailDialog.labelAgentId")} value={detail.agent_id} mono />
            <DetailRow label={t("agentDetailDialog.labelVersion")} value={detail.version} />
            <DetailRow label={t("agentDetailDialog.labelAuthor")} value={detail.author || "—"} />
            <DetailRow label={t("agentDetailDialog.labelDescription")} value={detail.description || "—"} />
            <DetailRow
              label={t("agentDetailDialog.labelStatus")}
              value={
                <span className="flex items-center gap-1.5">
                  <span
                    className={cn(
                      "inline-block h-2 w-2 rounded-full",
                      detail.alive ? "bg-[var(--color-accent)]" : "bg-surface-strong",
                    )}
                  />
                  {detail.alive ? t("agentDetailDialog.statusRunning") : t("agentDetailDialog.statusStopped")}
                </span>
              }
            />
            {detail.pid !== null && <DetailRow label={t("agentDetailDialog.labelPid")} value={String(detail.pid)} mono />}
            {detail.started_at && <DetailRow label={t("agentDetailDialog.labelStartedAt")} value={detail.started_at} />}
            <DetailRow label={t("agentDetailDialog.labelInstallPath")} value={detail.install_path} mono />
            {modelInfo && (
              <DetailRow
                label={t("agentDetailDialog.labelCurrentModel")}
                value={
                  <span className="flex items-center gap-1.5">
                    <span className="font-mono text-xs" style={{ color: "var(--color-accent)" }}>{modelInfo.model}</span>
                    <span className="text-10 text-text-tertiary">({modelInfo.provider})</span>
                  </span>
                }
              />
            )}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          <button
            onClick={onClose}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
          >
            Close
          </button>
        </div>
      </div>
    </div>
  );
}

function DetailRow({
  label,
  value,
  mono = false,
}: {
  label: string;
  value: React.ReactNode;
  mono?: boolean;
}) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-xs text-text-tertiary ">{label}</span>
      {typeof value === "string" ? (
        <span className={cn("text-text ", mono && "font-mono text-xs")}>
          {value}
        </span>
      ) : (
        value
      )}
    </div>
  );
}
