/**
 * Login gate for the always-on account system (ADR-076 §决策 6 / 7, ADR-114).
 *
 * Rendered by App.tsx whenever the resolved auth state is `logged_out`.
 * Two entry modes:
 *   - password login (default)
 *   - **first login** — an admin created the account without a password,
 *     handing the owner a one-time `invite_token`; the owner sets the
 *     first password here (§决策 6).
 *
 * Mounted only when `status === "logged_out"`; a fresh install goes
 * to the first-boot setup view instead (ADR-114).
 */

import { useState, type FormEvent } from "react";
import { ChevronDown } from "lucide-react";
import { useAuthStore } from "../../stores/authStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useTranslation } from "../../i18n/useTranslation";
import { StyledInput } from "../common/StyledInput";
import { GatewayAddressPicker } from "../connection/GatewayAddressPicker";
import { cn } from "../../lib/utils";
import brandMark from "../../../../../assets/brand-mark.svg";

type Mode = "password" | "invite";

export function LoginView() {
  const { t } = useTranslation();
  const login = useAuthStore((s) => s.login);
  const firstLogin = useAuthStore((s) => s.firstLogin);
  const gatewayUrl = useSettingsStore((s) => s.gatewayUrl);

  const [mode, setMode] = useState<Mode>("password");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [inviteToken, setInviteToken] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // The address section is COLLAPSED by default: this view only renders
  // once the boot pipeline resolved auth, so the current address is
  // connected and there is nothing to switch. A failed login auto-expands
  // it — a wrong password (or a wrong server) is exactly the moment the
  // user needs another address (ADR-115).
  const [pickerOpen, setPickerOpen] = useState(false);

  const canSubmit =
    !busy &&
    (mode === "password"
      ? username.trim().length > 0 && password.length > 0
      : inviteToken.trim().length > 0 && newPassword.length > 0);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!canSubmit) return;
    setBusy(true);
    setError(null);
    try {
      if (mode === "password") {
        await login(username.trim(), password);
      } else {
        await firstLogin(inviteToken.trim(), newPassword);
      }
    } catch {
      // Both actions store the Gateway message; re-read it for display so
      // the View stays a dumb renderer of store + local error state.
      setError(
        useAuthStore.getState().error ??
          t(mode === "password" ? "account.loginFailed" : "account.firstLoginFailed"),
      );
      // The escape hatch opens itself: a failed login is the moment the
      // user may need a different server address.
      setPickerOpen(true);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex h-screen w-screen flex-col items-center justify-center bg-page-bg">
      <form
        onSubmit={submit}
        className="flex w-full max-w-xs flex-col gap-3 rounded-md border border-border-outer bg-modal-surface p-6 shadow-xl"
      >
        {/* Brand mark — same SVG as SplashScreen / README, so the login
            dialog visually matches the rest of the app's identity. 177 keeps the
            ink the size it had at the old 192px width: the asset's frame is now
            artwork + a uniform 6px gutter, i.e. 364.25/396 of its former width
            (see assets/brand-mark.svg). */}
        <img
          src={brandMark}
          alt={t("splashScreen.altLogo")}
          width={177}
          className="mx-auto"
        />
        <h1 className="text-center text-sm font-semibold text-text">
          {t(mode === "password" ? "account.loginTitle" : "account.firstLoginTitle")}
        </h1>
        {/* Subtitle — invite mode only. "登录" + "登录后继续使用" restated
            the same thing twice on the password screen; the invite line
            ("输入管理员给你的邀请码，然后设置密码。") is an actual
            instruction, so it stays. */}
        {mode === "invite" && (
          <p className="text-center text-xs text-text-tertiary">
            {t("account.firstLoginSubtitle")}
          </p>
        )}

        {mode === "password" ? (
          <>
            <label className="flex flex-col gap-1 text-xs text-text-tertiary">
              {t("account.username")}
              <StyledInput
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                autoFocus
                autoComplete="username"
                spellCheck={false}
                disabled={busy}
              />
            </label>

            <label className="flex flex-col gap-1 text-xs text-text-tertiary">
              {t("account.password")}
              <StyledInput
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                autoComplete="current-password"
                disabled={busy}
              />
            </label>
          </>
        ) : (
          <>
            <label className="flex flex-col gap-1 text-xs text-text-tertiary">
              {t("account.inviteToken")}
              <StyledInput
                value={inviteToken}
                onChange={(e) => setInviteToken(e.target.value)}
                autoFocus
                spellCheck={false}
                disabled={busy}
              />
            </label>

            <label className="flex flex-col gap-1 text-xs text-text-tertiary">
              {t("account.newPassword")}
              <StyledInput
                type="password"
                value={newPassword}
                onChange={(e) => setNewPassword(e.target.value)}
                autoComplete="new-password"
                disabled={busy}
              />
            </label>
          </>
        )}

        {error && (
          <p role="alert" className="text-xs text-red-500">
            {error}
          </p>
        )}

        <button
          type="submit"
          disabled={!canSubmit}
          className="mt-1 rounded-md bg-accent-strong px-3 py-1.5 text-xs font-medium text-white transition-opacity disabled:opacity-50"
        >
          {busy
            ? t(mode === "password" ? "account.loggingIn" : "account.activating")
            : t(mode === "password" ? "account.login" : "account.activate")}
        </button>

        <button
          type="button"
          onClick={() => {
            setMode(mode === "password" ? "invite" : "password");
            setError(null);
          }}
          disabled={busy}
          className="text-xs text-[var(--color-accent)] hover:underline disabled:opacity-50"
        >
          {t(mode === "password" ? "account.useInvite" : "account.backToLogin")}
        </button>

        {/* ADR-115: the login gate must never be a dead end. Collapsed by
            default (the current address is connected — see `pickerOpen`);
            the summary row expands into the full picker. Inside the card,
            below a divider — one centered card, no second panel. */}
        <div className="mt-1 border-t border-border-divider pt-3">
          <button
            type="button"
            aria-expanded={pickerOpen}
            onClick={() => setPickerOpen((v) => !v)}
            className="flex w-full items-center gap-2 text-xs text-text-tertiary transition-colors hover:text-text-secondary"
          >
            <span className="shrink-0">{t("connection.serverAddress")}</span>
            <span className="min-w-0 flex-1 truncate text-left font-mono text-text-secondary">
              {gatewayUrl}
            </span>
            <ChevronDown
              aria-hidden="true"
              className={cn("h-3.5 w-3.5 shrink-0 transition-transform", pickerOpen && "rotate-180")}
            />
          </button>
          {pickerOpen && (
            <div className="mt-3">
              <GatewayAddressPicker
                compact
                busy={busy}
                onSelect={(url) => void useSettingsStore.getState().switchGateway(url)}
                onReset={() => void useSettingsStore.getState().resetConnectionSettings()}
              />
            </div>
          )}
        </div>
      </form>
    </div>
  );
}
