/**
 * Local-network layer of the gateway connectivity module.
 *
 * The Gateway usually runs on THIS machine. When the laptop wakes on a
 * new LAN (Wi-Fi hop, sleep/wake), the persisted Gateway URL still
 * carries the OLD local IP — a black hole: every request hangs until
 * the OS TCP timeout while a perfectly healthy Gateway keeps listening
 * on the new address (2026-10-09 incident: 4+ minutes of silent
 * outage). This module records the "host was one of this machine's own
 * IPs" fact and, once the host stops being local, silently re-points
 * the URL at loopback (preferred — immune to any future network
 * change) or the machine's new IP. A truly remote gateway never
 * matches the hint, so it is left to the normal unreachable flow
 * (hint + candidate list, see `gatewayConnectivity.ts`).
 *
 * Silent by design: the product requirement is "just connect", not
 * "ask the user which of their own addresses to use".
 */
import { invoke } from "@tauri-apps/api/core";
import { log } from "../logger";
import { probeGateways } from "../gateway-probe";
import { useChatStore } from "../../stores/chatStore";
import { useSettingsStore } from "../../stores/settingsStore";

/** Preferred auto-heal target: stable across every future network change. */
const LOOPBACK_HOST = "127.0.0.1";
/** localStorage key: the URL host last observed among this machine's IPs. */
const STORAGE_KEY_LOCAL_HOST_HINT = "acowork-gateway-local-host-hint";
/** After a failed probe round, wait this long before trying again. */
const RETRY_COOLDOWN_MS = 15_000;

let _switchInFlight = false;
let _cooldownUntil = 0;

export function isLoopbackHost(host: string): boolean {
  return host === "127.0.0.1" || host === "localhost" || host === "::1" || host === "[::1]";
}

/** Rebuild `urlStr` with a different host, keeping scheme + port. */
export function replaceUrlHost(urlStr: string, host: string): string | null {
  try {
    const u = new URL(urlStr);
    return `${u.protocol}//${host}${u.port ? `:${u.port}` : ""}`;
  } catch {
    return null;
  }
}

/** This machine's current non-loopback IPv4 addresses ([] when unavailable). */
export async function fetchLocalIpv4Addresses(): Promise<string[]> {
  try {
    return await invoke<string[]>("get_local_ipv4_addresses");
  } catch {
    return [];
  }
}

/** The IPv4 of the interface owning the default route — the address a LAN
 *  peer (phone) can actually reach. `fetchLocalIpv4Addresses` returns
 *  interfaces in enumeration order and virtual adapters (WSL vEthernet,
 *  security-suite NICs) can sort first, so it must NOT be used to pick a
 *  phone-reachable address on its own. `null` when no route exists. */
export async function fetchDefaultRouteIpv4(): Promise<string | null> {
  try {
    return await invoke<string | null>("get_default_route_ipv4");
  } catch {
    return null;
  }
}

export function readLocalHostHint(): string | null {
  try {
    return localStorage.getItem(STORAGE_KEY_LOCAL_HOST_HINT);
  } catch {
    return null;
  }
}

function writeLocalHostHint(host: string): void {
  try {
    localStorage.setItem(STORAGE_KEY_LOCAL_HOST_HINT, host);
  } catch {
    /* private mode / storage disabled — the guard simply never fires */
  }
}

/**
 * One auto-heal pass. Safe to call often (mount, `visibilitychange`,
 * `online`, and a 5 s tick while disconnected): cheap local invokes,
 * and every real action is guarded — single-flight, hint match,
 * recovery check, cooldown.
 */
export async function maybeAutoSwitchLocalGateway(): Promise<void> {
  if (_switchInFlight || Date.now() < _cooldownUntil) return;
  const { gatewayUrl, gatewayMode, setGatewayUrl } = useSettingsStore.getState();
  // Relay URLs are device domains, never local.
  if (gatewayMode === "relay") return;
  let host: string;
  try {
    host = new URL(gatewayUrl).hostname;
  } catch {
    return;
  }
  if (isLoopbackHost(host)) return;

  const ips = await fetchLocalIpv4Addresses();
  if (ips.includes(host)) {
    // The URL already points at this machine — remember the fact so a
    // future IP change is recognised as "the gateway is still here".
    writeLocalHostHint(host);
    return;
  }
  // Host is not local NOW. Only act when it provably WAS local (hint) —
  // otherwise this is a genuinely remote gateway, left untouched.
  if (readLocalHostHint() !== host) return;
  // Don't fight an in-flight recovery; only heal a down connection.
  if (useChatStore.getState().mqttConnected) return;
  if (ips.length === 0) return; // enumeration unavailable — nothing to probe

  _switchInFlight = true;
  try {
    const candidates = [LOOPBACK_HOST, ...ips]
      .map((h) => replaceUrlHost(gatewayUrl, h))
      .filter((u): u is string => u !== null);
    const results = await probeGateways(candidates);
    // Loopback wins whenever it answers — even if a LAN IP was faster:
    // it is the one address that survives every FUTURE network change.
    const loopbackUrl = replaceUrlHost(gatewayUrl, LOOPBACK_HOST);
    const winner =
      results.find((r) => r.ok && r.url === loopbackUrl) ??
      results.filter((r) => r.ok).sort((a, b) => a.latencyMs - b.latencyMs)[0];
    if (!winner) {
      // Nothing answered — the gateway may really be down. Back off and
      // let the normal unreachable hint + candidates flow take over.
      _cooldownUntil = Date.now() + RETRY_COOLDOWN_MS;
      return;
    }
    log.warn(
      `[gateway-connectivity] local gateway host ${host} left this machine — switching to ${winner.url}`,
    );
    // setGatewayUrl pushes the config to Rust (set_gateway_config +
    // connect_mqtt) and validates the session — the MQTT client
    // rebuilds against the new broker by design.
    setGatewayUrl(winner.url);
  } finally {
    _switchInFlight = false;
  }
}

/** Test-only: reset module-level guards between cases. */
export function _resetLocalNetworkForTests(): void {
  _switchInFlight = false;
  _cooldownUntil = 0;
}
