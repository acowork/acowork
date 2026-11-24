import { useState, useRef, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useTranslation } from "../../i18n/useTranslation";
import { ErrorBox } from "../common/ErrorBox";
import { cn } from "../../lib/utils";
import { StyledInput } from "../common/StyledInput";
import type { CloneMode, CloneResponse } from "../../lib/types";
import { Copy, Info } from "lucide-react";
import { useEscapeClose } from "../../hooks/useEscapeClose";

interface CloneDialogProps {
  open: boolean;
  /**
   * Source agent INSTANCE id — the route variable. ADR-073: the Gateway
   * clone route resolves through the installed table, so this is the
   * instance key, never the package id.
   */
  agentId: string;
  /**
   * Source agent PACKAGE id (`meta.agent_id`) — display identity and the
   * base for the suggested new id. A distinct prop from `agentId`
   * because the two answer different questions: routing vs. naming. Using
   * the instance here is what produced UUID-shaped "package" ids like
   * `8f7be9a6-….cloned-261007`.
   */
  packageId: string;
  /** Source agent display name */
  agentName: string;
  /** Called when cloning succeeds */
  onCloned: (result: CloneResponse) => void;
  onClose: () => void;
}

export function CloneDialog({
  open,
  agentId,
  packageId,
  agentName,
  onCloned,
  onClose,
}: CloneDialogProps) {
  const { t } = useTranslation();
  const [newAgentId, setNewAgentId] = useState("");
  const [newName, setNewName] = useState("");
  const [mode, setMode] = useState<CloneMode>("skeleton");
  const [cloning, setCloning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const modeDescriptions: Record<CloneMode, { label: string; desc: string }> = {
    skeleton: {
      label: t("cloneDialog.skeleton"),
      desc: t("cloneDialog.skeletonDesc"),
    },
    full: {
      label: t("cloneDialog.full"),
      desc: t("cloneDialog.fullDesc"),
    },
  };

  // A clone is a new INSTANCE of the same package (ADR-073): the unique
  // identity is `instance_id`, so the package id is free to repeat. The
  // only thing that must differ is the display name, which the backend
  // falls back to "<source name> (clone)" when left blank.
  useEffect(() => {
    if (open) {
      setNewAgentId(packageId);
      setNewName("");
      setError(null);
      const t = setTimeout(() => inputRef.current?.focus(), 50);
      return () => clearTimeout(t);
    }
  }, [open, packageId]);

  // Close on Escape
  useEscapeClose(open, onClose);

  const handleClone = async () => {
    const trimmed = newAgentId.trim();
    if (!trimmed) {
      setError(t("cloneDialog.errorEmptyId"));
      return;
    }
    setCloning(true);
    setError(null);
    try {
      const result = await invoke<CloneResponse>("clone_agent", {
        agentId,
        newAgentId: trimmed,
        newName: newName.trim(),
        mode,
      });
      onCloned(result);
    } catch (e) {
      setError(String(e));
    } finally {
      setCloning(false);
    }
  };

  if (!open) return null;

  const modeInfo = modeDescriptions[mode];

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      {/* Backdrop */}
      <div className="absolute inset-0 bg-modal-overlay" onClick={onClose} />

      {/* Dialog */}
      <div className="relative z-10 w-full max-w-lg rounded-md border border-border-outer bg-modal-surface shadow-xl">
        {/* Header */}
        <div className="flex items-center gap-2 border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <Copy className="h-5 w-5 text-text-tertiary " />
          <h2 className="text-sm font-semibold text-text ">
            {t("cloneDialog.title")}
          </h2>
        </div>

        {/* Body */}
        <div className="space-y-4 px-5 py-4">
          {/* Source info */}
          <div className="flex items-center gap-2 rounded-md bg-surface-subtle px-3 py-2 text-xs">
            <Info className="h-4 w-4 text-text-tertiary" />
            <span className="text-text-tertiary ">
              {t("cloneDialog.cloningFrom")}{" "}
            </span>
            <span className="font-medium text-text-secondary ">
              {agentName}
            </span>
            <span className="text-xs text-text-tertiary">({packageId})</span>
          </div>

          {/* New agent ID */}
          <div>
            <label className="mb-1.5 block text-xs font-medium text-text-tertiary ">
              {t("cloneDialog.newAgentIdLabel")}
            </label>
            <StyledInput
              ref={inputRef}
              type="text"
              value={newAgentId}
              onChange={(e) => {
                setNewAgentId(e.target.value);
                setError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") void handleClone();
              }}
              placeholder={t("cloneDialog.newAgentIdPlaceholder")}
              className=""
            />
            <p className="mt-1.5 text-xs text-text-quaternary">
              {t("cloneDialog.newAgentIdHint")}
            </p>
          </div>

          {/* New agent name — the only field that must differ from the
              source. Blank means "<source name> (clone)". */}
          <div>
            <label className="mb-1.5 block text-xs font-medium text-text-tertiary ">
              {t("cloneDialog.newNameLabel")}
            </label>
            <StyledInput
              type="text"
              value={newName}
              onChange={(e) => {
                setNewName(e.target.value);
                setError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") void handleClone();
              }}
              placeholder={t("cloneDialog.newNamePlaceholder")}
              className=""
            />
          </div>

          {/* Clone mode */}
          <div>
            <label className="mb-1.5 block text-xs font-medium text-text-tertiary ">
              {t("cloneDialog.cloneModeLabel")}
            </label>
            <div className="flex gap-2">
              {(["skeleton", "full"] as CloneMode[]).map((m) => (
                <button
                  key={m}
                  onClick={() => setMode(m)}
                  className={cn(
                    "flex-1 rounded-md border px-3 py-1.5 text-xs font-medium transition-colors",
                    mode === m
                      ? "border-border-outer bg-surface-strong text-text dark:text-text "
                      : "border-border-outer text-text-secondary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15",
                  )}
                >
                  {modeDescriptions[m].label}
                </button>
              ))}
            </div>
            <p className="mt-1.5 text-xs text-text-tertiary ">
              {modeInfo.desc}
            </p>
          </div>

          {/* Error */}
          {error && (
            <ErrorBox message={error} onClose={() => setError(null)} />
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          <button
            onClick={onClose}
            disabled={cloning}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
          >
            {t("common.cancel")}
          </button>
          <button
            onClick={handleClone}
            disabled={cloning || !newAgentId.trim()}
            className="flex items-center gap-2 rounded btn-accent px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
          >
            {cloning ? (
              <>
                <div className="h-3.5 w-3.5 animate-spin rounded-full border-2 border-white/30 border-t-white" />
                {t("cloneDialog.cloning")}
              </>
            ) : (
              <>
                <Copy className="h-3.5 w-3.5" />
                {t("cloneDialog.clone")}
              </>
            )}
          </button>
        </div>
      </div>
    </div>
  );
}
