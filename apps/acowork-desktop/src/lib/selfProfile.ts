/**
 * Access to the caller's own profile (ADR-076 §决策 3, ADR-114).
 *
 * ADR-114: the account system is **always present**, so `accounts.json` is
 * the single authority. The caller's own record is `GET /api/auth/me` (a
 * flat `AccountView`), and writes go to `PUT /api/users/{user_id}` with a
 * flat body. There is no anonymous "active user" concept — every caller is
 * its own account, and `GET /api/users` answers `{ accounts }` (admin-only
 * for non-self reads).
 *
 * The old local-profile branch — which read `{ users, version }`, picked
 * `is_active`, and wrote through `POST /api/users` — is gone. With no mode
 * left to select it, it could never be reached.
 */

import { useAuthStore } from "../stores/authStore";
import { getGatewayUrl } from "./config";
import type { BackendUserProfile, UpdateUserRequest, UserAccount } from "./types";

/**
 * Project an account record onto the profile shape the settings UI renders.
 *
 * `is_active` has no account-system counterpart (there is no active-user
 * selector — the caller *is* the user), but the UI reads it, so it is
 * synthesized as always-true.
 */
function accountToProfile(account: UserAccount): BackendUserProfile {
  return {
    user_id: account.user_id,
    display_name: account.display_name,
    language: account.language,
    timezone: account.timezone,
    city: account.city,
    country: account.country,
    occupation: account.occupation,
    communication_style: account.communication_style,
    custom: account.custom,
    created_at: account.created_at,
    updated_at: account.updated_at,
    is_active: true,
    avatar: account.avatar ?? null,
    builtin_avatar: account.builtin_avatar ?? null,
  };
}

/** `GET /api/auth/me` — the caller's own account (ADR-076 §决策 3). */
async function fetchMeAccount(gatewayUrl: string): Promise<UserAccount | null> {
  const token = useAuthStore.getState().accessToken;
  if (!token) return null;
  const resp = await fetch(`${gatewayUrl}/api/auth/me`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  if (!resp.ok) throw new Error(`Failed to fetch account: ${resp.status}`);
  return (await resp.json()) as UserAccount;
}

/** The caller's own profile, or `null` when there is no session yet. */
export async function fetchSelfProfile(
  gatewayUrl = getGatewayUrl(),
): Promise<BackendUserProfile | null> {
  const account = await fetchMeAccount(gatewayUrl);
  return account ? accountToProfile(account) : null;
}

/**
 * Patch the caller's own profile and return the stored record.
 *
 * PUTs the caller's account — deliberately with no `role` field, because
 * the Gateway only honours a role change for admins and a user must never
 * be able to promote themselves. The response is a flat `AccountView`.
 */
export async function updateSelfProfile(
  patch: UpdateUserRequest,
  gatewayUrl = getGatewayUrl(),
): Promise<BackendUserProfile> {
  const account = await fetchMeAccount(gatewayUrl);
  if (!account) throw new Error("Not signed in");
  const resp = await fetch(`${gatewayUrl}/api/users/${account.user_id}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(patch),
  });
  if (!resp.ok) {
    const err = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error(
      (err as { error?: string }).error ?? `Failed to update profile: ${resp.status}`,
    );
  }
  return accountToProfile((await resp.json()) as UserAccount);
}

/** The identity the onboarding wizard collects in its last-but-one step. */
export interface OnboardingIdentity {
  display_name: string;
  language: string;
  timezone: string;
  city?: string;
  occupation?: string;
}

/**
 * Persist the identity collected by the onboarding wizard.
 *
 * The account already exists — the boot order is (setup or) login, then
 * onboarding — so the wizard must *patch* it. The legacy call it replaces
 * issued `POST /api/users`, which created a second account nobody is
 * signed in as: the wizard reported success while the logged-in user's
 * profile stayed empty.
 */
export async function saveOnboardedIdentity(
  identity: OnboardingIdentity,
  gatewayUrl = getGatewayUrl(),
): Promise<void> {
  await updateSelfProfile(identity, gatewayUrl);
}
