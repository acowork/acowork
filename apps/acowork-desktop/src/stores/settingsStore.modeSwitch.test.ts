/**
 * Contract: the connection address is the ONLY input; the topology mode is
 * derived from it (ADR-115).
 *
 * The old model had two independent persisted knobs (`gatewayUrl` +
 * `gatewayMode`) that could disagree — a user could end up with
 * `mode=local` while the address pointed at a LAN host, and Desktop then
 * spawned a loopback child that nothing ever probed. These tests pin the
 * new invariant: nothing outside the URL (plus the explicit relay opt-in)
 * may influence `gatewayMode`.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

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

const REMOTE_URL = "http://192.168.3.67:19876";
const LOCAL_URL = "http://127.0.0.1:19876";
const LOCALHOST_ALIAS = "http://localhost:19876";
const LAN_IP_LOOPBACK = "http://127.0.0.2:19876";
const RELAY_URL = "https://c8cb2bed-ffd4-4821-a913-718618482157.relay.acowork.ai";

describe("settingsStore derived topology mode", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useSettingsStore.setState({
      gatewayUrl: LOCAL_URL,
      gatewayMode: "local",
      relayActive: false,
      gatewayUrlHistory: [],
      lastGoodUrl: null,
    });
  });

  it("a loopback address derives `local`; any other address derives `remote`", () => {
    useSettingsStore.getState().setGatewayUrl(REMOTE_URL);
    expect(useSettingsStore.getState().gatewayMode).toBe("remote");

    useSettingsStore.getState().setGatewayUrl(LOCAL_URL);
    expect(useSettingsStore.getState().gatewayMode).toBe("local");
  });

  it("loopback aliases (localhost, 127/8, [::1]) all derive `local`", () => {
    for (const url of [LOCALHOST_ALIAS, LAN_IP_LOOPBACK, "http://[::1]:19876"]) {
      useSettingsStore.getState().setGatewayUrl(url);
      expect(useSettingsStore.getState().gatewayMode, url).toBe("local");
    }
  });

  it("switching a plain address never leaves relay active", async () => {
    await useSettingsStore.getState().setRelayGateway(RELAY_URL);
    expect(useSettingsStore.getState().gatewayMode).toBe("relay");
    expect(useSettingsStore.getState().relayActive).toBe(true);

    await useSettingsStore.getState().switchGateway(REMOTE_URL);
    expect(useSettingsStore.getState().gatewayMode).toBe("remote");
    expect(useSettingsStore.getState().relayActive).toBe(false);
  });

  it("switchGateway drops the session and restarts the boot pipeline", async () => {
    await useSettingsStore.getState().switchGateway(REMOTE_URL);
    expect(clearSessionMock).toHaveBeenCalledTimes(1);
    expect(restartMock).toHaveBeenCalledTimes(1);
    expect(useSettingsStore.getState().gatewayUrl).toBe(REMOTE_URL);
  });

  it("the same address in switchGateway is still a deliberate reconnect", async () => {
    await useSettingsStore.getState().switchGateway(LOCAL_URL);
    expect(useSettingsStore.getState().gatewayUrl).toBe(LOCAL_URL);
    expect(restartMock).toHaveBeenCalledTimes(1);
  });

  it("trailing slashes are normalized so one address is one address", () => {
    useSettingsStore.getState().setGatewayUrl(`${REMOTE_URL}/`);
    expect(useSettingsStore.getState().gatewayUrl).toBe(REMOTE_URL);
  });

  it("resetConnectionSettings returns to localhost and forgets everything", async () => {
    useSettingsStore.setState({
      gatewayUrlHistory: [REMOTE_URL],
      lastGoodUrl: REMOTE_URL,
    });
    await useSettingsStore.getState().resetConnectionSettings();
    const s = useSettingsStore.getState();
    expect(s.gatewayUrl).toBe(LOCAL_URL);
    expect(s.gatewayMode).toBe("local");
    expect(s.gatewayUrlHistory).toEqual([]);
    expect(s.lastGoodUrl).toBeNull();
    expect(s.relayActive).toBe(false);
    expect(restartMock).toHaveBeenCalledTimes(1);
  });

  it("recordGatewayUrl promotes a connected address to last-good", () => {
    useSettingsStore.getState().recordGatewayUrl(REMOTE_URL);
    expect(useSettingsStore.getState().lastGoodUrl).toBe(REMOTE_URL);
    expect(useSettingsStore.getState().gatewayUrlHistory[0]).toBe(REMOTE_URL);
    // Recording history must never change the live target.
    expect(useSettingsStore.getState().gatewayUrl).toBe(LOCAL_URL);
  });
});
