/**
 * `switchGateway` is the ONE user-facing address change (ADR-115).
 *
 * It replaces the old `applyGatewayUrl` + `setGatewayMode` pair. Those two
 * knobs could disagree (relay mode + http URL was the visible symptom), and
 * the "fix" was a mode correction bolted onto the URL setter. Now the mode
 * is derived, so the only invariant left to pin is:
 *
 *   the `set_gateway_config` payload mode MUST equal the mode derived from
 *   the payload URL (plus the relay opt-in).
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { invoke } from "@tauri-apps/api/core";

const { restartMock, clearSessionMock } = vi.hoisted(() => ({
  restartMock: vi.fn(),
  clearSessionMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async () => ({})),
}));
vi.mock("./authStore", () => ({
  useAuthStore: {
    getState: () => ({
      onGatewayUrlChanged: vi.fn(async () => {}),
      clearLocalSession: clearSessionMock,
    }),
  },
}));
vi.mock("./connectionStore", () => ({
  useConnectionStore: { getState: () => ({ restart: restartMock }) },
}));

import { useSettingsStore } from "./settingsStore";

const RELAY_URL = "https://c8cb2bed-ffd4-4821-a913-718618482157.relay.acowork.ai";
const LAN_URL = "http://192.168.0.101:19876";
const LOCAL_URL = "http://127.0.0.1:19876";

function configPushes(): { mode: string; url: string }[] {
  return vi
    .mocked(invoke)
    .mock.calls.filter(([cmd]) => cmd === "set_gateway_config")
    .map(([, args]) => (args as { config: { mode: string; url: string } }).config);
}

describe("settingsStore.switchGateway", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useSettingsStore.setState({
      gatewayUrl: LOCAL_URL,
      gatewayMode: "local",
      relayActive: false,
    });
  });

  it("a LAN address is pushed as `remote`", async () => {
    await useSettingsStore.getState().switchGateway(LAN_URL);
    expect(configPushes()).toEqual([{ mode: "remote", url: LAN_URL }]);
  });

  it("a loopback address is pushed as `local` (so Rust may spawn)", async () => {
    await useSettingsStore.getState().switchGateway("http://localhost:19876");
    expect(configPushes()).toEqual([{ mode: "local", url: "http://localhost:19876" }]);
  });

  it("the relay entry is the only producer of `relay`", async () => {
    await useSettingsStore.getState().setRelayGateway(RELAY_URL);
    expect(configPushes()).toEqual([{ mode: "relay", url: RELAY_URL }]);
  });

  it("picking a plain address after relay pushes an http-safe mode, never relay+http", async () => {
    await useSettingsStore.getState().setRelayGateway(RELAY_URL);
    vi.clearAllMocks();
    await useSettingsStore.getState().switchGateway(LAN_URL);
    const pushes = configPushes();
    expect(pushes).toHaveLength(1);
    expect(pushes[0]).toEqual({ mode: "remote", url: LAN_URL });
  });

  it("an empty address is ignored (no push, no reboot)", async () => {
    await useSettingsStore.getState().switchGateway("   ");
    expect(configPushes()).toHaveLength(0);
    expect(restartMock).not.toHaveBeenCalled();
  });

  /**
   * Relay is TLS-only. `commitGatewayUrl` drops the opt-in for a non-https
   * address, so accepting one here would look like "relay applied" while the
   * store quietly connected as a plain remote Gateway — the caller's intent
   * and the resulting mode would disagree. Refuse outright instead.
   */
  it("the relay entry refuses a non-https address instead of downgrading", async () => {
    await useSettingsStore.getState().setRelayGateway(LAN_URL);
    expect(configPushes()).toHaveLength(0);
    expect(restartMock).not.toHaveBeenCalled();
    expect(useSettingsStore.getState().relayActive).toBe(false);
  });

  /**
   * A scheme-less address is not a Gateway URL — it is a host that `base_url`
   * can never resolve. Persisting one makes the NEXT launch start by trying
   * an address that cannot work, so the store refuses it and no reboot
   * happens (rebooting would drop the session over a value we just rejected).
   */
  it("a scheme-less address is refused (no push, no reboot, no state change)", async () => {
    const before = useSettingsStore.getState().gatewayUrl;
    await useSettingsStore.getState().switchGateway("192.168.0.101:19876");
    expect(configPushes()).toHaveLength(0);
    expect(restartMock).not.toHaveBeenCalled();
    expect(useSettingsStore.getState().gatewayUrl).toBe(before);

    await useSettingsStore.getState().switchGateway("abc");
    expect(configPushes()).toHaveLength(0);
    expect(useSettingsStore.getState().gatewayUrl).toBe(before);
  });

  it("each switch reboots exactly once", async () => {
    await useSettingsStore.getState().switchGateway(LAN_URL);
    await useSettingsStore.getState().switchGateway(LOCAL_URL);
    expect(restartMock).toHaveBeenCalledTimes(2);
  });
});
