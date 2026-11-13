/**
 * First-boot setup view (ADR-114).
 *
 * Rendered by App.tsx whenever `authStatus === "setup_required"` — no
 * administrator can log in yet. This is the Desktop half of the onboarding
 * flow: it drives `POST /api/auth/setup`, which activates the passwordless
 * `admin` seeded by the user service at boot and returns a token pair.
 *
 * The Gateway admits that call only from a **real loopback peer** (not from
 * a public bind), and its relay listener denies the path entirely, so a
 * remote client sees 403 `setup_local_only` and gets the CLI hint below.
 *
 * Polling stays for the other completion paths (`admin-setup` on the host,
 * the TTY prompt, or another local client): the view then continues
 * automatically into the normal login flow.
 */

import { useEffect, useState, type FormEvent } from "react";
import { ChevronDown } from "lucide-react";
import { useAuthStore } from "../../stores/authStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useTranslation } from "../../i18n/useTranslation";
import { AuthApiError } from "../../lib/auth-api";
import { StyledInput } from "../common/StyledInput";
import { GatewayAddressPicker } from "../connection/GatewayAddressPicker";
import { cn } from "../../lib/utils";
import brandMark from "../../../../../assets/brand-mark.svg";

/**
 * ADR-114: the server owns the `PasswordPolicy`, and it is configurable
 * (`[user].password_policy` in the user service TOML). Duplicating its
 * default here would either reject a password the server accepts or admit
 * one the server rejects, so the form only enforces local invariants
 * (non-empty, confirmed match) and surfaces the 422 policy message from
 * `POST /api/auth/setup` verbatim.
 */
export function SetupRequiredView() {
  const { t } = useTranslation();
  const completeSetup = useAuthStore((s) => s.completeSetup);
  const pollUntilSetupComplete = useAuthStore((s) => s.pollUntilSetupComplete);
  const stopSetupPoll = useAuthStore((s) => s.stopSetupPoll);
  const accountState = useAuthStore((s) => s.accountState);
  const gatewayUrl = useSettingsStore((s) => s.gatewayUrl);

  const [username, setUsername] = useState("admin");
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Address section collapsed by default (same reasoning as LoginView:
  // this view renders after the boot pipeline resolved auth, so the
  // current address is connected). A failed setup — above all the 403
  // `setup_local_only` from a remote Gateway — auto-expands it, because
  // that failure is only fixable by switching to localhost (ADR-115).
  const [pickerOpen, setPickerOpen] = useState(false);

  useEffect(() => {
    pollUntilSetupComplete();
    return () => stopSetupPoll();
  }, [pollUntilSetupComplete, stopSetupPoll]);

  const mismatch = confirm.length > 0 && password !== confirm;
  const canSubmit =
    !busy &&
    username.trim().length > 0 &&
    password.length > 0 &&
    confirm.length > 0 &&
    !mismatch;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!canSubmit) return;
    setBusy(true);
    setError(null);
    try {
      await completeSetup(username.trim(), password, displayName.trim() || undefined);
    } catch (err) {
      // A 403 is not necessarily "you are not on the Gateway host": a
      // stale snapshot can also answer `setup_required` during startup.
      // Only the explicit local-only code earns the CLI hint; every other
      // failure (422 policy, 409 already set up, 503 proxy not ready, 404
      // on the relay surface) must show its real reason.
      if (
        err instanceof AuthApiError &&
        err.status === 403 &&
        err.message === "setup_local_only"
      ) {
        setError(
          t("account.setupLocalOnly", {
            defaultValue:
              "First-boot setup is only allowed from the Gateway host itself. Use the Desktop on that machine, or run `acowork-gateway admin-setup` there.",
          }),
        );
      } else {
        setError(
          (err instanceof AuthApiError ? err.message : null) ??
            useAuthStore.getState().error ??
            t("account.setupFailed", { defaultValue: "Setup failed. Please try again." }),
        );
      }
      // The escape hatch opens itself: `setup_local_only` in particular is
      // only fixable by switching the server address.
      setPickerOpen(true);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex h-screen w-screen flex-col items-center justify-center bg-page-bg">
      <form
        onSubmit={submit}
        className="flex w-full max-w-sm flex-col gap-3 rounded-md border border-border-outer bg-modal-surface p-6 shadow-xl"
      >
        <img
          src={brandMark}
          alt={t("splashScreen.altLogo")}
          width={140}
          className="mx-auto"
        />
        <h1 className="text-center text-sm font-semibold text-text">
          {t("account.setupTitle", { defaultValue: "Set up your administrator account" })}
        </h1>
        <p className="text-center text-xs leading-relaxed text-text-tertiary">
          {accountState === "uninitialized"
            ? t("account.setupBodyUninitialized", {
                defaultValue:
                  "This is a fresh install with no accounts yet. Create the first administrator to start using the Gateway.",
              })
            : t("account.setupBodyNeedsSetup", {
                defaultValue:
                  "An administrator account has been created but has no password yet. Set one here to finish the first-boot setup.",
              })}
        </p>

        <label className="flex flex-col gap-1 text-xs text-text-tertiary">
          {t("account.username", { defaultValue: "Username" })}
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
          {t("account.displayName", { defaultValue: "Display name (optional)" })}
          <StyledInput
            value={displayName}
            onChange={(e) => setDisplayName(e.target.value)}
            autoComplete="name"
            disabled={busy}
          />
        </label>

        <label className="flex flex-col gap-1 text-xs text-text-tertiary">
          {t("account.newPassword", { defaultValue: "Password" })}
          <StyledInput
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoComplete="new-password"
            disabled={busy}
          />
        </label>

        <label className="flex flex-col gap-1 text-xs text-text-tertiary">
          {t("account.confirmPassword", { defaultValue: "Confirm password" })}
          <StyledInput
            type="password"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            autoComplete="new-password"
            disabled={busy}
          />
        </label>

        {mismatch && (
          <p className="text-xs text-red-500">
            {t("account.setupPasswordMismatch", { defaultValue: "Passwords do not match." })}
          </p>
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
            ? t("account.settingUp", { defaultValue: "Creating…" })
            : t("account.createAdmin", { defaultValue: "Create administrator" })}
        </button>

        <p className="text-xs leading-relaxed text-text-muted">
          {t("account.setupAlternative", {
            defaultValue:
              "Prefer the command line? Run `acowork-gateway admin-setup` on the Gateway host. This page continues automatically once setup is finished anywhere.",
          })}
        </p>

        {/* ADR-115: setup is also a connection gate. Collapsed by default
            (see `pickerOpen`); the summary row expands into the full
            picker. Inside the card, below a divider — one centered card. */}
        <div className="border-t border-border-divider pt-3">
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
