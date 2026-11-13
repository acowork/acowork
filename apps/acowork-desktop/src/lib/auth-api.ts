/**
 * Account-system HTTP client (ADR-076 §决策 3 / 6, ADR-114).
 *
 * Thin, side-effect-free wrappers over the Gateway `/api/auth/*` routes.
 * These calls deliberately use plain `fetch` with an explicit
 * `Authorization` header rather than the app-wide interceptor
 * ([authFetch.ts](authFetch.ts)) — the interceptor skips `/api/auth/*`
 * anyway, and the refresh call must never recurse into the 401→refresh
 * path. Keeping the token plumbing here (not in the store) means the
 * store only owns state, and error shapes stay in one place.
 *
 * ADR-114: the account system is always present. There is no mode in which
 * these routes 404 — what varies is the store's first-boot state, which
 * `/api/status` reports as `account_state`.
 */

import type { AccountSystemState, DirectoryUser, TokenPair, UserAccount } from "./types";

/**
 * Budget for the public auth-policy probe. The probe runs against a
 * possibly black-holed URL while the UI waits on it, so it must fail
 * on a product-level clock instead of the OS TCP retransmit schedule
 * (~21s on Windows). 5s matches the connectivity module's
 * user-facing unreachable budget.
 */
const AUTH_POLICY_TIMEOUT_MS = 5_000;

/** Error carrying the Gateway's HTTP status so callers can branch (401 vs 422). */
export class AuthApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "AuthApiError";
  }
}

/**
 * Pull the Gateway's error text out of a failed response.
 *
 * Exported (not auth-specific): the user-chat client
 * ([user-chat-api.ts](user-chat-api.ts)) reuses it, so the `{error}` /
 * `{detail}` shape stays decoded in exactly one place.
 */
export async function readError(resp: Response): Promise<string> {
  try {
    const body = await resp.json();
    if (body && typeof body === "object") {
      const detail = (body as Record<string, unknown>).detail;
      const error = (body as Record<string, unknown>).error;
      if (typeof detail === "string") return detail;
      if (typeof error === "string") return error;
    }
  } catch {
    // non-JSON body — fall through to the status text
  }
  return resp.statusText || `HTTP ${resp.status}`;
}

async function postJson(url: string, body: unknown, accessToken?: string): Promise<Response> {
  return fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      ...(accessToken ? { Authorization: `Bearer ${accessToken}` } : {}),
    },
    body: JSON.stringify(body),
  });
}

/**
 * Read the account system's first-boot state from the public `/api/status`
 * probe (ADR-114).
 *
 * A missing or unrecognised `account_state` is treated as `ready`: a
 * Gateway that has not answered yet (or predates ADR-114) must not send
 * the Desktop into the setup wizard. This mirrors the Gateway's own
 * fail-open default for a snapshot that has not arrived.
 *
 * `requiresSetup` is derived from the state rather than being a second
 * wire field — one source of truth, so the two can never disagree.
 *
 * Bounded by `AUTH_POLICY_TIMEOUT_MS`: a black-holed gateway URL (stale
 * LAN IP after a Wi-Fi hop) makes the raw fetch hang on the OS TCP
 * retransmit schedule (~21s on Windows). The AbortSignal turns that
 * into a fast, catchable rejection so auth probing can never pin the
 * UI; the connectivity module owns the user-facing recovery flow.
 */
export async function fetchAuthPolicy(
  gatewayUrl: string,
): Promise<{
  accountState: AccountSystemState;
  registrationOpen: boolean;
  requiresSetup: boolean;
}> {
  const resp = await fetch(`${gatewayUrl}/api/status`, {
    signal: AbortSignal.timeout(AUTH_POLICY_TIMEOUT_MS),
  });
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
  const data = (await resp.json()) as {
    account_state?: string;
    registration_open?: boolean;
  };
  const accountState: AccountSystemState =
    data.account_state === "uninitialized"
      ? "uninitialized"
      : data.account_state === "needs_setup"
        ? "needs_setup"
        : "ready";
  return {
    accountState,
    registrationOpen: data.registration_open === true,
    requiresSetup: accountState !== "ready",
  };
}

/**
 * ADR-114: create the first administrator from this machine.
 *
 * Public (no token yet) and gated by the Gateway on the **real peer
 * address** — a remote Desktop gets 403 `setup_local_only`, and the relay
 * listener denies the path entirely. Returns a token pair so the wizard
 * can enter the app without a second login round-trip.
 */
export interface SetupAdminBody {
  username: string;
  password: string;
  display_name?: string;
}

export async function setupRequest(
  gatewayUrl: string,
  body: SetupAdminBody,
): Promise<TokenPair> {
  const resp = await postJson(`${gatewayUrl}/api/auth/setup`, body);
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
  return (await resp.json()) as TokenPair;
}

export async function loginRequest(
  gatewayUrl: string,
  username: string,
  password: string,
): Promise<TokenPair> {
  const resp = await postJson(`${gatewayUrl}/api/auth/login`, { username, password });
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
  return (await resp.json()) as TokenPair;
}

export async function refreshRequest(
  gatewayUrl: string,
  refreshToken: string,
): Promise<TokenPair> {
  const resp = await postJson(`${gatewayUrl}/api/auth/refresh`, {
    refresh_token: refreshToken,
  });
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
  return (await resp.json()) as TokenPair;
}

/** Best-effort: revoke the refresh family. A dead token is not an error here. */
export async function logoutRequest(gatewayUrl: string, refreshToken: string): Promise<void> {
  await postJson(`${gatewayUrl}/api/auth/logout`, { refresh_token: refreshToken });
}

export async function changePasswordRequest(
  gatewayUrl: string,
  accessToken: string,
  oldPassword: string,
  newPassword: string,
): Promise<void> {
  const resp = await postJson(
    `${gatewayUrl}/api/auth/change-password`,
    { old_password: oldPassword, new_password: newPassword },
    accessToken,
  );
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
}

export async function fetchMe(gatewayUrl: string, accessToken: string): Promise<UserAccount> {
  const resp = await fetch(`${gatewayUrl}/api/auth/me`, {
    headers: { Authorization: `Bearer ${accessToken}` },
  });
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
  return (await resp.json()) as UserAccount;
}

/** Admin-only account list (`GET /api/users`, ADR-076 §决策 5). */
export async function fetchAccounts(
  gatewayUrl: string,
  accessToken: string,
): Promise<UserAccount[]> {
  const resp = await fetch(`${gatewayUrl}/api/users`, {
    headers: { Authorization: `Bearer ${accessToken}` },
  });
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
  const data = (await resp.json()) as { accounts?: UserAccount[] };
  return data.accounts ?? [];
}

/** Contact-picker source for any authenticated caller
 *  (`GET /api/users/directory`, ADR-076 §决策 8).
 *
 *  Backend deliberately strips `role` / timestamps / profile — non-admin
 *  callers get only the fields needed to name a recipient. The server
 *  already filters out the caller themselves and disabled accounts.
 */
export async function fetchDirectory(
  gatewayUrl: string,
  accessToken: string,
): Promise<DirectoryUser[]> {
  const resp = await fetch(`${gatewayUrl}/api/users/directory`, {
    headers: { Authorization: `Bearer ${accessToken}` },
  });
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
  const data = (await resp.json()) as { users?: DirectoryUser[] };
  return data.users ?? [];
}

/**
 * Self-service soft delete (ADR-076 §决策 6).
 *
 * `account_api::delete_account` resolves `{user_id}` and enforces
 * `self-or-admin` — there is no `"self"` literal, so the caller must pass
 * its own `user_id`.
 */
export async function deleteAccount(
  gatewayUrl: string,
  accessToken: string,
  userId: string,
): Promise<void> {
  const resp = await fetch(`${gatewayUrl}/api/users/${encodeURIComponent(userId)}`, {
    method: "DELETE",
    headers: { Authorization: `Bearer ${accessToken}` },
  });
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
}

/** ADR-076 §决策 6: consume a one-time `invite_token` and set the first password. */
export async function firstLoginRequest(
  gatewayUrl: string,
  inviteToken: string,
  newPassword: string,
): Promise<TokenPair> {
  const resp = await postJson(`${gatewayUrl}/api/auth/first-login`, {
    invite_token: inviteToken,
    new_password: newPassword,
  });
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
  return (await resp.json()) as TokenPair;
}

// ── Admin account management (ADR-076 §决策 5 / 6) ────────────────────

export interface CreateAccountBody {
  username: string;
  display_name: string;
  /** Omitted → inactive account + an `invite_token` for first-login. */
  password?: string;
}

export interface CreateAccountResult {
  account: UserAccount;
  /** Present only when no password was supplied. */
  invite_token?: string;
}

export async function createAccount(
  gatewayUrl: string,
  accessToken: string,
  body: CreateAccountBody,
): Promise<CreateAccountResult> {
  const resp = await postJson(`${gatewayUrl}/api/users`, body, accessToken);
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
  return (await resp.json()) as CreateAccountResult;
}

/** Admin: mint a fresh one-time `invite_token` (24h) for an account. */
export async function resetPassword(
  gatewayUrl: string,
  accessToken: string,
  userId: string,
): Promise<string> {
  const resp = await postJson(
    `${gatewayUrl}/api/users/${encodeURIComponent(userId)}/reset-password`,
    {},
    accessToken,
  );
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
  const data = (await resp.json()) as { invite_token: string };
  return data.invite_token;
}

/** Admin: change an account's role (`PUT /api/users/{id}`). */
export async function setRole(
  gatewayUrl: string,
  accessToken: string,
  userId: string,
  role: "user" | "admin",
): Promise<UserAccount> {
  const resp = await fetch(`${gatewayUrl}/api/users/${encodeURIComponent(userId)}`, {
    method: "PUT",
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${accessToken}`,
    },
    body: JSON.stringify({ role }),
  });
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
  return (await resp.json()) as UserAccount;
}

/** Admin: soft-delete another account (`POST /api/users/{id}/disable`). */
export async function disableAccount(
  gatewayUrl: string,
  accessToken: string,
  userId: string,
): Promise<void> {
  const resp = await postJson(
    `${gatewayUrl}/api/users/${encodeURIComponent(userId)}/disable`,
    {},
    accessToken,
  );
  if (!resp.ok) throw new AuthApiError(resp.status, await readError(resp));
}
