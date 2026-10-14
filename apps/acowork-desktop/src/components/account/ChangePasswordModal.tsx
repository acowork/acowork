/**
 * Change-password modal (ADR-076 §决策 6).
 *
 * `POST /api/auth/change-password` requires the old password and, on
 * success, the Gateway revokes the caller's whole refresh family — so the
 * store reloads into LoginView afterwards. This modal only collects the
 * two inputs and surfaces a policy/credential error.
 */

import { useEffect, useState, type FormEvent } from "react";
import { useAuthStore } from "../../stores/authStore";
import { useTranslation } from "../../i18n/useTranslation";
import { AuthApiError } from "../../lib/auth-api";
import { StyledInput } from "../common/StyledInput";

interface ChangePasswordModalProps {
  open: boolean;
  onClose: () => void;
}

export function ChangePasswordModal({ open, onClose }: ChangePasswordModalProps) {
  const { t } = useTranslation();
  const changePassword = useAuthStore((s) => s.changePassword);
  const [oldPassword, setOldPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) {
      setOldPassword("");
      setNewPassword("");
      setConfirm("");
      setError(null);
      setBusy(false);
    }
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [open, onClose]);

  if (!open) return null;

  const mismatch = confirm.length > 0 && confirm !== newPassword;
  const canSubmit =
    oldPassword.length > 0 && newPassword.length > 0 && !mismatch && !busy;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!canSubmit) return;
    setBusy(true);
    setError(null);
    try {
      await changePassword(oldPassword, newPassword);
      // On success the store reloads the app; nothing more to do here.
    } catch (err) {
      setError(
        err instanceof AuthApiError ? err.message : t("account.changePasswordFailed"),
      );
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-modal-overlay" onClick={onClose} />
      <form
        onSubmit={submit}
        className="relative z-10 flex w-full max-w-sm flex-col rounded-md border border-border-outer bg-modal-surface shadow-xl"
        role="dialog"
        aria-modal="true"
      >
        <h3 className="border-b border-border-divider px-5 py-3 text-sm font-semibold text-text">
          {t("account.changePasswordTitle")}
        </h3>

        <div className="flex flex-col gap-3 px-5 py-4">
          <label className="flex flex-col gap-1 text-11 text-text-tertiary">
            {t("account.oldPassword")}
            <StyledInput
              type="password"
              value={oldPassword}
              onChange={(e) => setOldPassword(e.target.value)}
              autoComplete="current-password"
              autoFocus
              disabled={busy}
            />
          </label>
          <label className="flex flex-col gap-1 text-11 text-text-tertiary">
            {t("account.newPassword")}
            <StyledInput
              type="password"
              value={newPassword}
              onChange={(e) => setNewPassword(e.target.value)}
              autoComplete="new-password"
              disabled={busy}
            />
          </label>
          <label className="flex flex-col gap-1 text-11 text-text-tertiary">
            {t("account.confirmPassword")}
            <StyledInput
              type="password"
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
              autoComplete="new-password"
              disabled={busy}
            />
          </label>
          {mismatch && (
            <p className="text-11 text-red-500">{t("account.passwordMismatch")}</p>
          )}
          {error && (
            <p role="alert" className="text-11 text-red-500">
              {error}
            </p>
          )}
        </div>

        <div className="flex justify-end gap-2 border-t border-border-divider px-5 py-3">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-3 py-1.5 text-xs text-text-tertiary hover:bg-hover-overlay"
          >
            {t("common.cancel")}
          </button>
          <button
            type="submit"
            disabled={!canSubmit}
            className="rounded-md bg-accent-strong px-3 py-1.5 text-xs font-medium text-white transition-opacity disabled:opacity-50"
          >
            {busy ? t("account.saving") : t("account.changePassword")}
          </button>
        </div>
      </form>
    </div>
  );
}
