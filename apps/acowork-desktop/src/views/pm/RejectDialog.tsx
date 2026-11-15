/**
 * RejectDialog — 拒绝任务确认对话框（T2-11）。
 *
 * 对齐 UX 设计 §3.6：拒绝前二次确认 + 可选填理由。
 * 理由通过 reviewTask(_comment) 透传服务端（ReviewTaskRequest._comment）。
 */

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { StyledTextarea } from "../../components/common/StyledInput";

interface RejectDialogProps {
  open: boolean;
  taskTitle: string;
  onConfirm: (reason?: string) => void;
  onCancel: () => void;
}

export function RejectDialog({ open, taskTitle, onConfirm, onCancel }: RejectDialogProps) {
  const { t } = useTranslation();
  const [reason, setReason] = useState("");
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (!open) return;
    setReason("");
    textareaRef.current?.focus();
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [open, onCancel]);

  if (!open) return null;

  const handleConfirm = () => {
    onConfirm(reason.trim() || undefined);
  };

  return (
    <div className="fixed inset-0 z-[60] flex items-center justify-center">
      <div className="absolute inset-0 bg-modal-overlay" onClick={onCancel} />
      <div
        className="relative z-10 w-full max-w-md rounded-md border border-border-outer bg-modal-surface p-6 shadow-xl"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="reject-title"
        aria-describedby="reject-desc"
      >
        <h3 id="reject-title" className="text-sm font-semibold">
          {t("pm.reject.title")}
        </h3>
        <p id="reject-desc" className="mt-1.5 text-xs text-text-tertiary ">
          {t("pm.reject.desc")} "{taskTitle}"?
        </p>

        <div className="mt-4">
          <label className="mb-1 block text-11 font-medium text-text-tertiary ">
            {t("pm.reject.reason")} <span className="text-text-tertiary">({t("pm.reject.optional")})</span>
          </label>
          <StyledTextarea
            ref={textareaRef}
            value={reason}
            onChange={(e) => setReason(e.target.value)}
            rows={3}
            placeholder={t("pm.reject.reasonPlaceholder")}
          />
        </div>

        <div className="mt-5 flex justify-end gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
          >
            {t("common.cancel")}
          </button>
          <button
            type="button"
            onClick={handleConfirm}
            className="rounded-md bg-red-600 px-4 py-1.5 text-xs font-medium text-white hover:bg-red-500 dark:bg-red-700 dark:hover:bg-red-600"
          >
            {t("pm.reject.confirm")}
          </button>
        </div>
      </div>
    </div>
  );
}
