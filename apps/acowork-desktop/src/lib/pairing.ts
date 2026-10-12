/**
 * QR pairing (方案③) — Desktop side.
 *
 * The signed-in Desktop mints a short-lived, single-use pairing token from the
 * Gateway (`POST /api/auth/pair/create`) and encodes it, together with a
 * phone-reachable Gateway base URL, into a QR image. The phone scans it, redeems
 * the token at the (public) `POST /api/auth/pair/redeem`, and receives a fresh
 * token pair — no password, no long-lived secret ever leaves the Desktop.
 *
 * Security posture (see the backend `AuthService::{create,redeem}_pairing`):
 *   - the QR carries ONLY a 60-second, one-time random token + the base URL;
 *   - it never encodes the account name, the password, nor the refresh token;
 *   - a leaked QR is worth at most one login within ~60s, and the operator can
 *     always invalidate by changing the password (kills every refresh family).
 *
 * Base-URL derivation: when the persisted Gateway URL is loopback the phone
 * cannot dial it, so we swap the host to this machine's LAN IPv4 (the same
 * enumeration the connectivity guard uses). A URL that already carries a LAN
 * IP or a relay hostname is used verbatim.
 */
import QRCode from "qrcode";
import { useAuthStore } from "../stores/authStore";
import { getGatewayUrl, isGatewayLocal } from "./config";
import { fetchLocalIpv4Addresses } from "./connectivity/localNetwork";

/** QR payload schema version — the mobile parser branches on this. */
export const PAIR_QR_SCHEMA = 1;

/** A ready-to-render pairing QR. */
export interface PairingQr {
  /** data-URL PNG, bound for an `<img src>`. */
  dataUrl: string;
  /** the base URL baked into the QR (shown under the code for manual entry). */
  baseUrl: string;
  /** wall-clock ms when the token stops being redeemable. */
  expiresAt: number;
}

/** Thrown when there is no phone-reachable address to encode (loopback Desktop
 *  with no LAN interface enumerated). The UI falls back to manual entry. */
export class NoLanAddressError extends Error {
  constructor() {
    super("no-lan-address");
    this.name = "NoLanAddressError";
  }
}

/**
 * Resolve the base URL a phone on the same network should dial.
 *
 * Loopback → swap to this machine's first LAN IPv4 (throws `NoLanAddressError`
 * when none). Anything else (a LAN IP already, or a remote/relay host) is used
 * as-is. Trailing slash is stripped so callers can append paths safely.
 */
export async function resolvePairingBaseUrl(): Promise<string> {
  const url = getGatewayUrl();
  if (!isGatewayLocal()) return trimTrailingSlash(url);
  const ips = await fetchLocalIpv4Addresses();
  if (ips.length === 0) throw new NoLanAddressError();
  const u = new URL(url);
  u.hostname = ips[0];
  return trimTrailingSlash(u.toString());
}

function trimTrailingSlash(s: string): string {
  return s.endsWith("/") ? s.slice(0, -1) : s;
}

/**
 * Mint a fresh pairing token and render it as a QR data-URL.
 *
 * The create call is authenticated as the signed-in user (the Gateway derives
 * the account from the bearer token — the token we return is bound to that
 * identity server-side, so the phone cannot choose which account to become).
 */
export async function createPairingQr(): Promise<PairingQr> {
  const token = useAuthStore.getState().accessToken;
  if (!token) throw new Error("not-logged-in");
  const baseUrl = await resolvePairingBaseUrl();

  const resp = await fetch(`${getGatewayUrl()}/api/auth/pair/create`, {
    method: "POST",
    headers: { Authorization: `Bearer ${token}` },
  });
  if (!resp.ok) throw new Error(`pair/create failed: ${resp.status}`);
  const data = (await resp.json()) as { pairing_token: string; expires_in: number };

  const payload = JSON.stringify({
    v: PAIR_QR_SCHEMA,
    gw: baseUrl,
    pt: data.pairing_token,
  });
  const dataUrl = await QRCode.toDataURL(payload, {
    margin: 1,
    width: 320,
    errorCorrectionLevel: "M",
  });
  return {
    dataUrl,
    baseUrl,
    expiresAt: Date.now() + data.expires_in * 1000,
  };
}
