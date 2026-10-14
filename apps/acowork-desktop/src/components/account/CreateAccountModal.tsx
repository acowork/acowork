/**
 * Admin "create account" modal (ADR-076 §决策 5 / 6).
 *
 * Leaving the password blank produces an inactive account plus a one-time
 * `invite_token` the owner uses for first login — the parent surfaces that
 * token via `InviteTokenModal`. Password-policy validation is left to the
 * Gateway (a 422 carries the policy message) so the rule lives in exactly
 * one place.
 */

import { useEffect, useState, type FormEvent } from "react";
import { useAuthStore } from "../../stores/authStore";
import { useTranslation } from "../../i18n/useTranslation";
import { AuthApiError, createAccount, type CreateAccountResult } from "../../lib/auth-api";
import { getGatewayUrl } from "../../lib/config";
import { StyledInput } from "../common/StyledInput";

interface CreateAccountModalProps {
  open: boolean;
  onClose: () => void;
  onCreated: (result: CreateAccountResult) => void;
}

export function CreateAccountModal({ open, onClose, onCreated }: CreateAccountModalProps) {
  const { t } = useTranslation();
  const [username, setUsername] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) {
      setUsername("");
      setDisplayName("");
      setPassword("");
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

  const canSubmit = username.trim().length > 0 && !busy;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!canSubmit) return;
    const accessToken = useAuthStore.getState().accessToken;
    if (!accessToken) return;
    setBusy(true);
    setError(null);
    try {
      const result = await createAccount(getGatewayUrl(), accessToken, {
        username: username.trim(),
        display_name: displayName.trim() || username.trim(),
        ...(password ? { password } : {}),
      });
      onCreated(result);
      onClose();
    } catch (err) {
      setError(err instanceof AuthApiError ? err.message : t("account.createFailed"));
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
          {t("account.createAccountTitle")}
        </h3>

        <div className="flex flex-col gap-3 px-5 py-4">
          <label className="flex flex-col gap-1 text-11 text-text-tertiary">
            {t("account.username")}
            <StyledInput
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              autoFocus
              spellCheck={false}
              disabled={busy}
            />
          </label>
          <label className="flex flex-col gap-1 text-11 text-text-tertiary">
            {t("account.displayName")}
            <StyledInput
              value={displayName}
              onChange={(e) => setDisplayName(e.target.value)}
              disabled={busy}
            />
          </label>
          <label className="flex flex-col gap-1 text-11 text-text-tertiary">
            {t("account.initialPassword")}
            <StyledInput
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete="new-password"
              disabled={busy}
            />
          </label>
          <p className="text-10 text-text-tertiary">{t("account.inviteHint")}</p>
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
            {busy ? t("account.saving") : t("account.create")}
          </button>
        </div>
      </form>
    </div>
  );
}
