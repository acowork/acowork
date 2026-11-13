/**
 * Account session state (ADR-076 §决策 3 / 6 / 7, ADR-114).
 *
 * Owns the *UI-facing* session: the account system's first-boot state, the
 * current account, the login gate, and the error/setup states the views
 * bind to.
 *
 * It deliberately does NOT own the token pair. That lives in
 * `lib/session/tokens.ts`, the single rotation source; this store keeps a
 * read-only mirror of the access token because a dozen components bind to
 * `accessToken` and need to re-render when it changes. `refreshToken` is not
 * mirrored — nothing outside the module has any business holding it.
 *
 * ADR-114: there is no "no-account-system" state. The account system is
 * always present; `accountState` only describes how far along its
 * first-boot lifecycle it is. On a fresh install the Desktop shows the
 * setup wizard instead of the login form, and completes it over HTTP from
 * the Gateway host.
 */

import { create } from "zustand";
import { getGatewayUrl } from "../lib/config";
import { log } from "../lib/logger";
import {
  AuthApiError,
  changePasswordRequest,
  deleteAccount,
  fetchAuthPolicy,
  fetchMe,
  firstLoginRequest,
  loginRequest,
  setupRequest,
} from "../lib/auth-api";
import {
  acquire,
  adopt,
  clear,
  onSessionEnd,
  onTokenChange,
  reportRejected,
  restore,
  signOut,
} from "../lib/session/tokens";
import type { AccountSystemState, AuthState, TokenPair, UserAccount } from "../lib/types";
import { useUserProfileStore } from "./userProfileStore";
import { setPrefScope } from "../lib/prefsScope";
import { RECOVERY_RELOAD_FLAG } from "../lib/recoveryReload";

/** Reload the webview so every store + MQTT listener reboots clean (§决策 7).
 *  Sets the `acowork_recovery_reload` flag so `App.tsx` skips the
 *  SplashScreen: the Gateway process is per-machine, not per-account, so
 *  switching accounts must not re-run `bootGateway` + the /health poll +
 *  the 1.5s minimum-splash linger. The recovery branch already re-registers
 *  the MQTT / fs / doc listeners that the reload destroys, which is exactly
 *  what we need here too. Without this, every account switch flashes
 *  "Connecting to Gateway..." for at least MIN_SPLASH_MS while the Gateway
 *  (already up) is re-probed for no reason. */
function reloadApp(): void {
  try {
    sessionStorage.setItem(RECOVERY_RELOAD_FLAG, "1");
    log.info("[reloadApp] setItem OK, raw=" + JSON.stringify(sessionStorage.getItem(RECOVERY_RELOAD_FLAG)) + ", reloading…");
    window.location.reload();
  } catch (err) {
    log.warn("[reloadApp] reload failed:", err);
  }
}

interface AuthStore {
  /** ADR-114: the account system's first-boot state; `"unknown"` until
   *  `/api/status` answers. Never derived from the network bind. */
  accountState: AccountSystemState | "unknown";
  /**
   * ADR-076 §决策 6: `[multi_user].registration_open`. When set, a logged-in
   * non-admin may create accounts (the Gateway still forces `role: user`), so
   * the sidebar offers the invite affordance to everyone.
   */
  registrationOpen: boolean;
  /**
   * ADR-114: while `true`, the Desktop renders the first-boot setup view
   * instead of the login form. Derived from `accountState !== "ready"`.
   * The store polls `/api/status` while this flag is set so a setup done
   * on another client (or via the CLI on the host) is picked up too.
   */
  setupRequired: boolean;
  /** App-level gate state. */
  status: AuthState;
  /** The caller's own account (redacted), once known. */
  account: UserAccount | null;
  /**
   * Read-only mirror of the module's access token. Components bind to it and
   * must re-render on rotation; nothing here decides when it changes.
   */
  accessToken: string | null;
  /** Last login error message, for the LoginView. */
  error: string | null;
  /**
   * ADR-076 §决策 5 / §决策 7: admin-only "view another user's sessions"
   * filter. When set, session listing appends `?as_user=<id>` (a read-only
   * view — the Gateway rejects `as_user` on writes).
   */
  viewAsUserId: string | null;
  setViewAsUserId: (userId: string | null) => void;
  /** ADR-076 §决策 12 v2: timer handle for the `/api/status` poll that
   * waits out first-boot setup on the Gateway host. `null` = no poll
   * in flight. */
  _setupPollHandle: ReturnType<typeof setInterval> | null;

  /** Resolve the account state and restore a stored session. Call once,
   *  after the Gateway is reachable. */
  init: () => Promise<void>;

  /**
   * Validate + re-resolve the session against a Gateway URL the user
   * just switched to (WiFi hop, LAN move, alias edit). Called by
   * `settingsStore.setGatewayUrl` whenever the URL actually changes.
   *
   * Probe-only: 1 round-trip to `GET /api/auth/me` on the new URL
   * with the existing access token.
   *
   *   - 200 OK          → keep session (same Gateway behind an alias).
   *                       Mode / registrationOpen / setupRequired are
   *                       unchanged, so init() is intentionally NOT
   *                       re-run: re-running would re-read tokens from
   *                       localStorage and clobber our in-memory
   *                       session with `logged_out` whenever localStorage
   *                       happens to be empty.
   *   - 401             → drop session (different Gateway, different
   *                       signing key) and re-run init() so the login
   *                       gate reflects the new deployment policy.
   *                       The existing zustand subscription in
   *                       `gatewayAuthBridge.ts` mirrors the `null`
   *                       access token into Rust, releasing any
   *                       in-flight `gateway-auth-required` waiter
   *                       (ADR-076 §决策 3) — no extra wiring required.
   *   - network error   → keep session, do NOT re-init. We can't
   *                       resolve the new Gateway's policy anyway,
   *                       and re-init would clobber the in-memory
   *                       session. The natural 401→refresh ladder in
   *                       `authFetch.ts` will recover when the
   *                       network is back.
   *
   * When called with no in-memory session (the user was on LoginView
   * when the URL changed), the probe is skipped and init() runs to
   * re-resolve the account state for the new URL.
   */
  onGatewayUrlChanged: (newUrl: string, oldUrl: string) => Promise<void>;

  /**
   * ADR-076 §决策 12 v2: poll `/api/status` every 5 s while
   * `setupRequired` is `true`. The moment the Gateway flips
   * `account_state` to `ready`, stop polling and re-run `init()` so
   * the rest of the auth flow takes over. Idempotent — calling it
   * twice does not spawn two pollers.
   */
  pollUntilSetupComplete: () => void;
  /** Stop the setup-completion poller, if any. Safe to call when no
   * poller is running. */
  stopSetupPoll: () => void;
  login: (username: string, password: string) => Promise<void>;
  /**
   * ADR-114: create the first administrator from this machine and enter
   * the app with the returned token pair. Only reachable while
   * `status === "setup_required"`; a remote client is refused by the
   * Gateway with 403 `setup_local_only`.
   */
  completeSetup: (username: string, password: string, displayName?: string) => Promise<void>;
  /** ADR-076 §决策 6: consume an invite token and set the first password. */
  firstLogin: (inviteToken: string, newPassword: string) => Promise<void>;
  /** Revoke the session, clear tokens, and reload into LoginView. */
  logout: () => Promise<void>;
  /** ADR-076 §决策 7: switch account == sign out then log in as someone else. */
  switchAccount: () => Promise<void>;
  /** Change own password; the Gateway revokes all families, so this also reloads. */
  changePassword: (oldPassword: string, newPassword: string) => Promise<void>;
  /** Soft-delete own account (§决策 6), then reload into LoginView. */
  deleteSelf: () => Promise<void>;
  /** Drop the in-memory session without touching the server. */
  clearLocalSession: () => void;
}

function app(account: UserAccount | null): void {
  // §决策 7: the account is the authority; userProfileStore keeps the
  // presentation mirror the top-bar avatar reads.
  //
  // Move the preference scope onto this account before mirroring, so the
  // user-level prefs (theme / accent / font size / width / opacity / language)
  // that `settingsStore` re-reads are the ones belonging to *this* user.
  setPrefScope(account?.user_id ?? null);
  if (!account) return;
  useUserProfileStore.getState().setProfile({
    displayName: account.display_name,
    backendAvatarUrl: account.avatar ?? null,
    backendBuiltinAvatarId: account.builtin_avatar ?? null,
  });
}

export const useAuthStore = create<AuthStore>((set, get) => ({
  accountState: "unknown",
  registrationOpen: false,
  setupRequired: false,
  status: "unknown",
  account: null,
  accessToken: null,
  error: null,
  viewAsUserId: null,
  _setupPollHandle: null,

  init: async () => {
    const url = getGatewayUrl();
    try {
      const policy = await fetchAuthPolicy(url);
      set({
        accountState: policy.accountState,
        registrationOpen: policy.registrationOpen,
        setupRequired: policy.requiresSetup,
      });
    } catch (err) {
      log.warn("[authStore] failed to resolve the account state:", err);
      // Gateway not answering the probe — leave `unknown` so the UI stays
      // out of the way, but do not gate the user behind a login.
      set({ accountState: "unknown", status: "unknown" });
      return;
    }

    // ADR-114: no login-capable administrator yet. Even with a valid token
    // in localStorage, every other `/api/*` route answers 403 until setup
    // completes, so show the first-boot wizard and start polling for a
    // setup performed elsewhere (CLI on the host, another local client).
    if (get().setupRequired) {
      set({ status: "setup_required" });
      get().pollUntilSetupComplete();
      return;
    }

    if (!restore()) {
      set({ status: "logged_out" });
      return;
    }

    // Ask the module for a token that is usable *now* — it rotates first if
    // the stored one is about to expire, so a boot at minute 14 of the TTL
    // does not walk straight into a 401.
    const token = await acquire();
    if (!token) {
      set({ status: "logged_out" });
      return;
    }

    try {
      const account = await fetchMe(url, token);
      set({ account, status: "logged_in" });
      app(account);
    } catch (err) {
      if (err instanceof AuthApiError && err.status === 401) {
        const fresh = await reportRejected(token);
        if (!fresh) return; // the module ended the session; see below
        try {
          const account = await fetchMe(url, fresh);
          set({ account, status: "logged_in" });
          app(account);
        } catch (retryErr) {
          log.warn("[authStore] /me failed after rotation:", retryErr);
          get().clearLocalSession();
        }
      } else {
        log.warn("[authStore] session restore failed:", err);
        get().clearLocalSession();
      }
    }
  },

  onGatewayUrlChanged: async (newUrl, oldUrl) => {
    if (newUrl === oldUrl) return;
    const accessToken = get().accessToken;
    // No session to validate: skip the probe (no Authorization header
    // to send). Just re-resolve the account state so LoginView's "wrong
    // credentials" state clears and setup_required transitions take
    // effect. init() will see no stored tokens → status=logged_out.
    if (!accessToken) {
      await get().init();
      return;
    }
    // 1 round-trip probe. Uses fetchMe directly (NOT through
    // installAuthFetchInterceptor) to avoid a recursive 401→refresh
    // loop on the very request that is supposed to validate the
    // token.
    try {
      await fetchMe(newUrl, accessToken);
      // Same Gateway behind an alias — session survives. We
      // deliberately do NOT re-run init() here: init() reads
      // tokens from localStorage (which may have been cleared by
      // a prior clearLocalSession or simply absent in tests), and
      // re-running it would clobber our in-memory session with
      // logged_out. Mode / registrationOpen / setupRequired are
      // unchanged when talking to the same Gateway behind an alias.
      return;
    } catch (err) {
      if (err instanceof AuthApiError && err.status === 401) {
        // Different Gateway's signing key. Drop the session; the
        // existing subscription on authStore.accessToken mirrors
        // null into Rust, releasing any in-flight
        // `gateway-auth-required` waiter (ADR-076 §决策 3). Then
        // re-init() against the new Gateway so the login gate
        // reflects the new account state (setup / login).
        get().clearLocalSession();
        await get().init();
        return;
      }
      // Network unreachable / 5xx — don't drop a possibly-valid
      // session on a flaky probe. Do NOT re-init either: a network
      // error there means we can't even resolve the new Gateway's
      // account state, and re-init would clobber our in-memory session.
      // Leave the store as-is; later 401s will trip the natural
      // interceptor refresh ladder when the network is back.
      log.warn("[authStore] onGatewayUrlChanged probe failed (keeping session):", err);
    }
  },

  completeSetup: async (username, password, displayName) => {
    const url = getGatewayUrl();
    set({ error: null });
    let pair: TokenPair;
    try {
      pair = await setupRequest(url, {
        username,
        password,
        ...(displayName ? { display_name: displayName } : {}),
      });
    } catch (err) {
      const message =
        err instanceof AuthApiError ? err.message : "Unable to reach the Gateway";
      set({ error: message });
      throw err;
    }

    adopt(pair);
    get().stopSetupPoll();

    // The Gateway's restricted-mode gate reads a health snapshot that lags
    // the user service by one poll, so an immediate `/api/auth/me` could
    // still answer 403 `setup_required`. Wait for `/api/status` to agree
    // before making authenticated calls, and remember whether it ever did.
    let gatewayReady = false;
    for (let attempt = 0; attempt < 20; attempt += 1) {
      try {
        const policy = await fetchAuthPolicy(url);
        if (!policy.requiresSetup) {
          gatewayReady = true;
          break;
        }
      } catch {
        // Gateway may be mid-restart; keep waiting.
      }
      await new Promise((resolve) => setTimeout(resolve, 250));
    }

    // Only claim `logged_in` once the account itself resolved. A `logged_in`
    // status with `account === null` leaves half the app uninitialised, and
    // the old code produced exactly that when the status poll timed out or
    // `/me` failed (it merely logged a warning).
    let account: UserAccount | null = null;
    let failure: unknown = null;
    for (let attempt = 0; attempt < 3; attempt += 1) {
      try {
        account = await fetchMe(url, pair.access_token);
        failure = null;
        break;
      } catch (err) {
        failure = err;
        await new Promise((resolve) => setTimeout(resolve, 300));
      }
    }

    if (account) {
      set({
        accountState: "ready",
        setupRequired: false,
        status: "logged_in",
        account,
        error: null,
      });
      app(account);
      return;
    }

    // Setup already succeeded on the server, so a second attempt would 409 —
    // the wizard is not a recovery path. Drop the local session and send the
    // user to the login form with the credentials they just chose.
    log.warn("[authStore] setup succeeded but the session could not be established:", {
      gatewayReady,
      failure,
    });
    clear();
    set({
      accountState: "ready",
      setupRequired: false,
      status: "logged_out",
      account: null,
      error: gatewayReady
        ? "Administrator created, but the session could not be loaded. Please sign in."
        : "Administrator created, but the Gateway has not reported ready yet. Please sign in.",
    });
  },

  login: async (username, password) => {
    const url = getGatewayUrl();
    set({ error: null });
    let pair: TokenPair;
    try {
      pair = await loginRequest(url, username, password);
    } catch (err) {
      // ADR-114 (2026-11-14): a 403 whose error code is `setup_required`
      // is the restricted gate answering — no login-capable admin exists,
      // so the login form should never have been reachable. Route to the
      // first-boot wizard instead of printing the raw error code: the boot
      // probe can outrun the user-service snapshot and land the user here
      // on a machine that has NO accounts at all.
      if (
        err instanceof AuthApiError &&
        err.status === 403 &&
        err.message === "setup_required"
      ) {
        set({
          accountState: "needs_setup",
          setupRequired: true,
          status: "setup_required",
          error: null,
        });
        get().pollUntilSetupComplete();
        return;
      }
      const message =
        err instanceof AuthApiError ? err.message : "Unable to reach the Gateway";
      set({ error: message });
      throw err;
    }
    // Hand the pair to the module; `accessToken` below is only the mirror
    // that `onTokenChange` keeps in sync.
    adopt(pair);
    set({ accountState: "ready", status: "logged_in", error: null });
    try {
      const account = await fetchMe(url, pair.access_token);
      set({ account });
      app(account);
    } catch (err) {
      log.warn("[authStore] logged in but /me failed:", err);
    }
  },

  firstLogin: async (inviteToken, newPassword) => {
    const url = getGatewayUrl();
    set({ error: null });
    let pair: TokenPair;
    try {
      pair = await firstLoginRequest(url, inviteToken, newPassword);
    } catch (err) {
      // Same restricted-gate conversion as `login` — an invite cannot be
      // redeemed while no login-capable admin exists, and the raw error
      // code is not a user-facing message.
      if (
        err instanceof AuthApiError &&
        err.status === 403 &&
        err.message === "setup_required"
      ) {
        set({
          accountState: "needs_setup",
          setupRequired: true,
          status: "setup_required",
          error: null,
        });
        get().pollUntilSetupComplete();
        return;
      }
      const message =
        err instanceof AuthApiError ? err.message : "Unable to reach the Gateway";
      set({ error: message });
      throw err;
    }
    adopt(pair);
    set({ accountState: "ready", status: "logged_in", error: null });
    try {
      const account = await fetchMe(url, pair.access_token);
      set({ account });
      app(account);
    } catch (err) {
      log.warn("[authStore] first login but /me failed:", err);
    }
  },

  logout: async () => {
    // The module owns revocation. `signOut()` drops the local pair
    // synchronously (which mirrors `accessToken` to null through the
    // subscription below) and fires the server revoke without awaiting it —
    // which is what the synchronous reload here needs anyway.
    void signOut();
    set({ status: "logged_out", account: null });
    // Reload FIRST, before any await: the set() above already scheduled a
    // React re-render that will briefly hit the SplashScreen/LoginView
    // before window.location.reload() can take over. Calling reloadApp()
    // synchronously here — before awaiting the server revoke — collapses
    // that interim render and lets App.tsx's `isRecoveryReload` branch
    // land directly on LoginView with a single paint.
    // ponytail: known ceiling — the server revoke becomes best-effort
    // fire-and-forget; an in-flight request may be aborted by the reload.
    // Refresh-token TTL bounds the risk window. Upgrade path: move revoke
    // to a Tauri Rust command that survives the webview reload.
    reloadApp();
  },

  switchAccount: async () => {
    await get().logout();
  },

  changePassword: async (oldPassword, newPassword) => {
    const accessToken = get().accessToken;
    if (!accessToken) throw new AuthApiError(401, "not logged in");
    await changePasswordRequest(getGatewayUrl(), accessToken, oldPassword, newPassword);
    // §决策 6.2: all refresh families are revoked — a fresh login is required.
    // Clear local tokens BEFORE reload: init() runs on the freshly mounted
    // component and would otherwise restore the session from localStorage
    // before the server-side revocation takes effect. Then setState + reload
    // — setState schedules an interim React render that the synchronous
    // reloadApp() overrides, so App.tsx's recovery branch lands directly on
    // LoginView with a single paint.
    clear();
    set({ status: "logged_out", account: null });
    reloadApp();
  },

  deleteSelf: async () => {
    const { accessToken, account } = get();
    if (!accessToken || !account) throw new AuthApiError(401, "not logged in");
    await deleteAccount(getGatewayUrl(), accessToken, account.user_id);
    // Clear local tokens before reload — see comment in changePassword.
    clear();
    set({ status: "logged_out", account: null });
    reloadApp();
  },

  clearLocalSession: () => {
    clear();
    set({
      status: "logged_out",
      account: null,
      viewAsUserId: null,
    });
  },

  // ADR-114: poll loop for the first-boot setup state. The
  // poll is intentionally light — it does NOT touch the network beyond
  // the public `/api/status` probe — so a long-running setup (operator
  // typing at the Gateway's TTY) costs us nothing.
  pollUntilSetupComplete: () => {
    if (get()._setupPollHandle !== null) return;
    const handle = setInterval(async () => {
      // Re-read setupRequired at tick-time — `stopSetupPoll` clears it.
      if (!get().setupRequired) {
        get().stopSetupPoll();
        return;
      }
      try {
        const policy = await fetchAuthPolicy(getGatewayUrl());
        set({
          accountState: policy.accountState,
          setupRequired: policy.requiresSetup,
          registrationOpen: policy.registrationOpen,
        });
        if (!policy.requiresSetup) {
          get().stopSetupPoll();
          // Re-enter `init()` — same path the splash takes on a fresh
          // boot, just with `setupRequired` now false. Existing
          // localStorage token (if any) will be restored.
          await get().init();
        }
      } catch (err) {
        // Gateway might be mid-restart while the operator applies
        // changes. Log and keep polling.
        log.warn("[authStore] setup-completion probe failed:", err);
      }
    }, 5000);
    set({ _setupPollHandle: handle });
  },

  stopSetupPoll: () => {
    const handle = get()._setupPollHandle;
    if (handle !== null) {
      clearInterval(handle);
      set({ _setupPollHandle: null });
    }
  },

  setViewAsUserId: (userId) => {
    set({ viewAsUserId: userId });
  },
}));

// ── Token-module wiring ───────────────────────────────────────────────
//
// The token pair itself — persistence, expiry bookkeeping, rotation timing,
// the single-flight guard, and the dead-session verdict — lives in
// `lib/session/tokens.ts`. This store is a *subscriber*: it mirrors the
// access token for the components that bind to it, and drops the session
// state when the module declares it over.
//
// The rotation transport is NOT wired here. Installing it as a module-load
// side effect made every other consumer depend on this store being imported
// first; it is now wired explicitly in `main.tsx` via
// `lib/session/wiring.ts`. These two subscriptions are safe as side effects
// because they only concern this store's own state.

// Read-only mirror — rotation decisions are not this store's.
onTokenChange((token) => {
  if (useAuthStore.getState().accessToken !== token) {
    useAuthStore.setState({ accessToken: token });
  }
});

// The module's verdict: the rotation was refused and no retry will help.
// Everything that a dead session invalidates — status, account, the admin
// view-as filter — is dropped here, once, instead of at each call site that
// used to discover it independently.
onSessionEnd(() => {
  log.info("[authStore] session ended by the token module — clearing local state");
  useAuthStore.getState().clearLocalSession();
});
