/**
 * Single source of truth for "which Gateway is this Desktop talking to".
 *
 * ADR-115 (Desktop connection model): there is ONE user-facing connection
 * setting — the Gateway address. `local` vs `remote` is not a choice; it is
 * a *consequence* of whether that address resolves to this machine. The
 * Rust backend still needs the three-way `GatewayMode` (MQTT transport,
 * relay ACL, owned-process cleanup), so we derive it here instead of
 * letting the user write it into two independent knobs that can disagree.
 *
 * Pure module: no store imports, so `config.ts`, `settingsStore.ts`, the
 * address picker and the boot path can all share the same verdict.
 */
import { DEFAULT_GATEWAY_URL } from "./defaults";
import type { GatewayMode } from "./types";

/** The fixed address a Desktop-spawned Gateway listens on. */
export const LOCAL_GATEWAY_URL = DEFAULT_GATEWAY_URL;

/** Trim + drop trailing slashes so `http://host:19876/` and the bare form
 *  are the same address everywhere (store, Rust base_url, history LRU). */
export function normalizeGatewayUrl(raw: string): string {
  return (raw ?? "").trim().replace(/\/+$/, "");
}

function isLoopbackHost(host: string): boolean {
  const h = host.toLowerCase().replace(/^\[|\]$/g, "");
  if (h === "localhost" || h === "::1" || h === "0:0:0:0:0:0:0:1") return true;
  // 127/8 — not just 127.0.0.1. Windows/browsers accept 127.0.0.2 etc.
  if (/^127(?:\.\d{1,3}){3}$/.test(h)) return true;
  // IPv4-mapped IPv6 loopback. Two spellings must both count, because
  // `new URL()` NORMALIZES the dotted form: `http://[::ffff:127.0.0.1]`
  // reports its hostname as `[::ffff:7f00:1]`. Only the mapped 127/8 range
  // (7f00:: – 7fff:ffff) is loopback; `::ffff:c0a8:10a` (192.168.1.10) is a
  // real LAN peer and must stay remote. Mirrors `is_loopback_peer()` in the
  // Gateway (ADR-114 §2.3) so both sides of the wire agree.
  if (/^::ffff:127(?:\.\d{1,3}){3}$/.test(h)) return true;
  if (/^::ffff:7f[0-9a-f]{2}:[0-9a-f]{1,4}$/.test(h)) return true;
  return false;
}

/**
 * Does this URL point at the machine Desktop itself runs on?
 *
 * This is the ONLY criterion deciding whether Desktop may spawn a local
 * Gateway. Using a persisted mode flag here was the root cause of the
 * "mode=local but URL=remote" deadlock: Rust spawned on loopback while
 * every probe went to the LAN host.
 */
export function isLoopbackGatewayUrl(url: string): boolean {
  const normalized = normalizeGatewayUrl(url);
  if (!normalized) return false;
  try {
    return isLoopbackHost(new URL(normalized).hostname);
  } catch {
    // Unparseable / scheme-less input ("192.168.1.10:19876"): fall back to
    // a manual host extraction so the verdict still fails closed (remote).
    const withoutScheme = normalized.replace(/^https?:\/\//i, "");
    const authority = withoutScheme.split("/")[0];
    // `[::1]:19876` → `[::1]`; `host:19876` → `host`.
    const host = authority.startsWith("[")
      ? authority.slice(0, authority.indexOf("]") + 1)
      : authority.split(":")[0];
    return isLoopbackHost(host);
  }
}

/**
 * Derive the internal topology mode. `relayActive` is the only explicit
 * input and is set exclusively by the dedicated relay entry point.
 */
export function deriveGatewayMode(url: string, relayActive: boolean): GatewayMode {
  if (relayActive) return "relay";
  return isLoopbackGatewayUrl(url) ? "local" : "remote";
}

/** Relay device domains are TLS-only — `relay_mqtt_wss_url` in chat_mqtt.rs
 *  refuses to derive a broker URL from anything else, so an `http://` address
 *  under relay mode is a permanently dead MQTT connection (HTTP health still
 *  answers, which is what made it look "connected"). */
export function isHttpsUrl(url: string): boolean {
  return /^https:\/\//i.test(normalizeGatewayUrl(url));
}

/**
 * Does this look like a Gateway address at all?
 *
 * Every consumer of `gatewayUrl` feeds it straight into `fetch`/`invoke` as
 * `base_url`, so a bare host (`192.168.1.5:19876`) or a stray keystroke
 * (`abc`) becomes an address that can never answer — and, once persisted, is
 * what the next launch tries first. `new URL()` would accept many of those
 * only after a guess about the scheme, so the check is explicit: an
 * `http://` / `https://` prefix is the whole contract.
 */
export function hasHttpScheme(url: string): boolean {
  return /^https?:\/\//i.test(normalizeGatewayUrl(url));
}

/**
 * Addresses offered by the picker, most useful first. `localhost` is
 * ALWAYS present — it is an ordinary server address, not a hidden mode.
 * Scheme-less entries are dropped: they can only come from stale storage,
 * and offering one would let the user pick an address that cannot connect.
 */
export function gatewayAddressOptions(
  current: string,
  history: string[],
  lastGoodUrl: string | null,
): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  for (const raw of [LOCAL_GATEWAY_URL, lastGoodUrl, current, ...history]) {
    const url = normalizeGatewayUrl(raw ?? "");
    if (!url || seen.has(url) || !hasHttpScheme(url)) continue;
    seen.add(url);
    out.push(url);
  }
  return out;
}
