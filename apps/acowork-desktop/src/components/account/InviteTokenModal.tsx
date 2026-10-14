/**
 * One-time invite-token display (ADR-076 §决策 6).
 *
 * Shown after an admin creates a password-less account or resets a
 * password. The token is the only way the owner can complete first
 * login, so it must be copyable and shown exactly once (the Gateway
 * stores only its SHA-256).
 */

import { useState } from "react";
import { Check, Copy } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { copyText } from "../../lib/clipboard";
import { StyledInput } from "../common/StyledInput";

interface InviteTokenModalProps {
  open: boolean;
  token: string;
  /** Account the token belongs to, for the explanatory line. */
  username?: string;
  onClose: () => void;
}

export function InviteTokenModal({ open, token, username, onClose }: InviteTokenModalProps) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);

  if (!open) return null;

  const copy = async () => {
    const ok = await copyText(token);
    if (ok) {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-modal-overlay" onClick={onClose} />
      <div
        className="relative z-10 flex w-full max-w-sm flex-col rounded-md border border-border-outer bg-modal-surface shadow-xl"
        role="dialog"
        aria-modal="true"
      >
        <h3 className="border-b border-border-divider px-5 py-3 text-sm font-semibold text-text">
          {t("account.inviteTokenTitle")}
        </h3>

        <div className="flex flex-col gap-3 px-5 py-4">
          <p className="text-11 text-text-tertiary">
            {username
              ? t("account.inviteTokenHintFor", { name: username })
              : t("account.inviteTokenHint")}
          </p>
          <div className="flex items-center gap-2">
            <StyledInput readOnly value={token} fontMono onFocus={(e) => e.target.select()} />
            <button
              type="button"
              onClick={copy}
              className="flex shrink-0 items-center gap-1 rounded-md border border-border-outer px-2 py-1.5 text-xs text-text-secondary hover:bg-hover-overlay"
            >
              {copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
              {copied ? t("account.copied") : t("account.copy")}
            </button>
          </div>
        </div>

        <div className="flex justify-end border-t border-border-divider px-5 py-3">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md bg-accent-strong px-3 py-1.5 text-xs font-medium text-white"
          >
            {t("account.done")}
          </button>
        </div>
      </div>
    </div>
  );
}
